//! Where features keep the things that must not sit in a file.
//!
//! Passwords, tokens, session cookies. No feature talks to a platform
//! keychain itself: it asks for its own [`Secrets`] handle and reads and
//! writes through that. Two reasons, and the second is the one that will
//! matter later:
//!
//! 1. **Keys are built in one place.** A feature names its corner once;
//!    every call after that passes only the account id or the token name.
//!    Two features cannot collide, and nobody has to remember the format.
//! 2. **The store underneath is replaceable.** There is one today, the OS
//!    keychain ([`Keychain`]), and that is exactly why the seam is worth
//!    having now: a passphrase-encrypted file for machines without a
//!    keyring, or the Android worker's own store, plugs in at
//!    [`install`] without any feature learning about it.
//!
//! # What this promises, and what it deliberately does not
//!
//! - **"No entry" and "cannot reach the store" are different answers.**
//!   `Ok(None)` means there is nothing stored; `Err` means the store did
//!   not answer. Collapsing the two is how a locked keyring turns into
//!   "you never logged in" and a sync quietly stops. [`SecretError`]
//!   splits the second case further, because the two halves want
//!   different handling: a store that cannot be reached opens the circuit
//!   breaker, a store that answered with a failure does not.
//! - **Values are never logged**, and errors never quote them.
//! - **No listing.** The trait cannot enumerate keys, because not every
//!   backend can (a keyring entry is found, not browsed). The one place
//!   that needs every CalDAV password at once -- the Android background
//!   worker -- reads its own store directly and hands them over, see
//!   `features::background`.
//! - **A caller says whether a person is waiting** ([`Intent`]), and the
//!   store decides what that is worth. A file store would answer
//!   instantly and can ignore it; only the keychain has a prompt that
//!   somebody might be typing into.
//! - **A missing store is an error, never `Ok(None)`.** Otherwise
//!   forgetting [`install`] in some entry point would look exactly like a
//!   device where the user never logged in -- silent, and wrong. The same
//!   holds for a keychain worker that died: `Err`, never an empty answer.
//!
//! # Cost, and why it is managed here
//!
//! Reaching a keychain is IO, and on Linux it is IPC to another process.
//! It used to be worse than that: every read opened its own D-Bus
//! connection, and a burst of those is what crashed gnome-keyring (see
//! `tauri_plugin_securestore::linux`). Three things keep the traffic
//! sane, and all three live in this file:
//!
//! - **One worker.** Every access is serialised through a single thread
//!   that owns the store, so at most one keychain call is ever in flight.
//! - **One read per key at a time.** Concurrent readers of the same key
//!   share one answer, which is what keeps the startup burst -- the
//!   account list, the subscription list, the first sync pass and the
//!   session restore all asking at once -- from being four reads of the
//!   same password.
//! - **A circuit breaker.** A store that did not answer is not asked
//!   again by background callers for a minute. Without it, serialising
//!   would turn one ten-second stall into one per queued reader.
//!
//! Features still should not read a secret in a loop: once per account
//! per sync pass, kept for that pass (see how the CalDAV sync builds one
//! client per account).

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, oneshot};

/// Whether somebody is sitting in front of this request.
///
/// The only thing it changes is how long the request may take before it
/// is given up on -- which matters because the one slow case, a locked
/// keyring, ends in a dialog that a person may be typing into. Making a
/// startup read wait that long would be a freeze; cutting a login off
/// after ten seconds would be rude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Nobody asked for this: a startup restore, a periodic sync, a
    /// capability check behind a list.
    Background,
    /// The direct result of a click: logging in, saving a password,
    /// reconnecting. Bypasses the circuit breaker, because "try again" is
    /// exactly what the user just said.
    Interactive,
}

/// Long enough for a keychain that answers in milliseconds, short enough
/// that a user does not read it as a freeze.
const BACKGROUND_DEADLINE: Duration = Duration::from_secs(10);

/// The budget for a person typing their keyring password.
const INTERACTIVE_DEADLINE: Duration = Duration::from_secs(60);

/// How long background callers stop asking after the store failed to
/// answer. Long enough that a queue of startup reads does not each pay
/// the full deadline, short enough that a keyring unlocked in the
/// meantime is picked up again without a restart.
const BREAKER_WINDOW: Duration = Duration::from_secs(60);

impl Intent {
    fn deadline(self) -> Duration {
        match self {
            Intent::Background => BACKGROUND_DEADLINE,
            Intent::Interactive => INTERACTIVE_DEADLINE,
        }
    }
}

/// Why a store did not answer with a value.
///
/// The split exists for one decision: [`Unavailable`](SecretError::Unavailable)
/// and [`Timeout`](SecretError::Timeout) mean the store could not be
/// reached and asking again immediately is pointless, so they open the
/// circuit breaker. [`Backend`](SecretError::Backend) means it answered,
/// and answered with a failure -- that is about this one request, and the
/// next one may well succeed.
///
/// "Nothing stored" is not in here at all: that is `Ok(None)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// No keychain to talk to: no Secret Service on the bus, a daemon
    /// that died, a keyring locked with nobody able to unlock it.
    Unavailable(String),
    /// The store was asked and did not answer within the deadline.
    Timeout(u64),
    /// The store answered, and what it said was a failure.
    Backend(String),
}

impl SecretError {
    /// Whether this means "the store could not be reached", which is the
    /// only kind of failure worth pausing on.
    fn store_did_not_answer(&self) -> bool {
        matches!(self, SecretError::Unavailable(_) | SecretError::Timeout(_))
    }
}

impl Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The raw error is a D-Bus or platform message that says
            // nothing a user can act on -- most often "dbus-launch
            // terminated abnormally", which means only that the
            // environment has no keychain service at all. That is a setup
            // problem, not a nobo problem, and the message should say so.
            SecretError::Unavailable(raw) => write!(
                f,
                "Kein Schlüsselbund verfügbar -- Passwörter können in dieser Umgebung nicht sicher gespeichert werden. \
Unter Linux braucht es einen laufenden Secret Service (z. B. gnome-keyring) auf dem D-Bus-Session-Bus. ({raw})"
            ),
            SecretError::Timeout(seconds) => write!(
                f,
                "Der Schlüsselbund antwortet nicht ({seconds}s). Unter Linux heißt das meist: \
der Schlüsselbund ist gesperrt und es kann kein Entsperr-Dialog angezeigt werden. \
Schlüsselbund entsperren und erneut versuchen."
            ),
            SecretError::Backend(raw) => {
                write!(f, "{}", crate::t!("Passwort konnte nicht im Schlüsselbund gespeichert werden: {raw}", "Could not save the password in the keychain: {raw}"))
            }
        }
    }
}

/// Somewhere secrets can be kept. Implemented once, by [`Keychain`].
#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    /// For logs: which store answered, or did not.
    fn name(&self) -> &'static str;

    /// The value stored under KEY, or `Ok(None)` if there is none. `Err`
    /// only when the store could not answer -- see the module doc on why
    /// those must stay apart.
    async fn load(&self, key: &str, intent: Intent) -> Result<Option<String>, SecretError>;

    /// Stores VALUE under KEY, replacing whatever was there.
    async fn save(&self, key: &str, value: &str, intent: Intent) -> Result<(), SecretError>;

    /// Removes KEY. Removing something that is not there is success:
    /// callers use this to make sure a secret is gone.
    async fn delete(&self, key: &str, intent: Intent) -> Result<(), SecretError>;
}

static STORE: RwLock<Option<Arc<dyn SecretStore>>> = RwLock::new(None);

/// Installs the store every feature will use, at startup.
///
/// The last one installed wins, and that is not academic: on Android the
/// process outlives the window, so a relaunch runs the setup again with a
/// fresh `AppHandle` -- keeping the first store would leave every
/// keychain call pointing at an app instance that is gone.
///
/// Swapping is safe because a store owns nothing: it is a handle to
/// somewhere secrets already are. What it does invalidate is everything
/// this module remembers ABOUT a store -- which legacy names it has
/// already found empty, above all -- so that is dropped here. A new store
/// is a new set of facts.
pub fn install(store: impl SecretStore + 'static) {
    let name = store.name();
    forget_what_a_store_told_us();
    match STORE.write() {
        Ok(mut slot) => {
            if slot.as_ref().is_some_and(|current| current.name() != name) {
                log::info!("replacing the secret store with '{name}'");
            }
            *slot = Some(Arc::new(store));
        }
        // Poisoned means a previous holder panicked mid-write; nothing
        // here can fix that, and pretending to have installed would be
        // worse than saying so.
        Err(e) => log::error!("could not install the secret store '{name}': {e}"),
    }
}

/// An owning handle, so no call holds the lock while it waits on a
/// keychain -- a locked keyring would otherwise block every other
/// feature's read for the duration of its timeout.
fn store() -> Result<Arc<dyn SecretStore>, String> {
    STORE
        .read()
        .map_err(|_| "the secret store is unusable after a panic".to_string())?
        .clone()
        // Loud on purpose: see the module doc.
        .ok_or_else(|| "no secret store installed on this device".to_string())
}

// ---------------------------------------------------------------------
// One read per key at a time
// ---------------------------------------------------------------------

type SharedRead = Result<Option<String>, SecretError>;

/// The reads currently in flight, by full key.
///
/// Not a cache: nothing is kept once the read is done. It exists so that
/// four callers asking for the same password in the same instant -- which
/// is exactly what app startup does -- produce one keychain access and
/// one migration, instead of four of each racing over the same entry.
fn reads_in_flight() -> &'static Mutex<HashMap<String, broadcast::Sender<SharedRead>>> {
    static IN_FLIGHT: OnceLock<Mutex<HashMap<String, broadcast::Sender<SharedRead>>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// Legacy keys already looked up and found empty in this process run.
///
/// Only this app writes entries under a legacy name, so once a lookup
/// came back empty it stays empty for as long as the app runs. Without
/// this, every read of a key that has nothing stored would pay the
/// legacy lookup again -- forever, not just while a rename is being
/// migrated -- and that is the common case for an account whose password
/// lives on another device.
fn legacy_names_known_empty() -> &'static Mutex<HashSet<String>> {
    static EMPTY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    EMPTY.get_or_init(Default::default)
}

fn forget_what_a_store_told_us() {
    if let Ok(mut empty) = legacy_names_known_empty().lock() {
        empty.clear();
    }
}

fn legacy_name_is_known_empty(legacy_key: &str) -> bool {
    legacy_names_known_empty()
        .lock()
        .is_ok_and(|known| known.contains(legacy_key))
}

fn remember_legacy_name_is_empty(legacy_key: &str) {
    if let Ok(mut known) = legacy_names_known_empty().lock() {
        known.insert(legacy_key.to_string());
    }
}

/// Either "you are doing this read" or "somebody else already is".
enum Flight {
    Leader(broadcast::Sender<SharedRead>),
    Follower(broadcast::Receiver<SharedRead>),
}

fn join_or_lead(key: &str) -> Flight {
    let Ok(mut in_flight) = reads_in_flight().lock() else {
        // A poisoned map costs deduplication, never correctness: leading
        // without announcing it just means somebody may read twice.
        let (sender, _) = broadcast::channel(1);
        return Flight::Leader(sender);
    };
    match in_flight.get(key) {
        Some(leader) => Flight::Follower(leader.subscribe()),
        None => {
            let (sender, _) = broadcast::channel(1);
            in_flight.insert(key.to_string(), sender.clone());
            Flight::Leader(sender)
        }
    }
}

/// Takes the key out of the in-flight map however the read ends --
/// returned, failed, or dropped because the caller gave up.
struct LeadingRead(String);

impl Drop for LeadingRead {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = reads_in_flight().lock() {
            in_flight.remove(&self.0);
        }
    }
}

/// One feature's corner of the store.
///
/// A feature declares one of these next to the code that uses it:
///
/// ```ignore
/// // features/caldav/mod.rs
/// pub const PASSWORDS: Secrets = Secrets::new("caldav_password:");
/// ```
///
/// The prefix is part of the on-disk format. Changing it does not migrate
/// anything -- it orphans every secret already stored under the old one,
/// and (see the module doc) nothing can enumerate them to clean up. Pick
/// it once.
pub struct Secrets {
    prefix: &'static str,
    legacy: Option<&'static str>,
}

impl Secrets {
    pub const fn new(prefix: &'static str) -> Self {
        Secrets { prefix, legacy: None }
    }

    /// The same corner, under a name it used to have.
    ///
    /// Renaming is not a startup migration (see core::migrations on why
    /// it refuses to touch secrets): a keychain can be locked, slow or
    /// absent exactly when the app starts, and nothing may enumerate what
    /// is in it. So the move happens read-through -- the first load that
    /// finds nothing under the new name looks under the old one, moves
    /// what it finds and answers with it.
    ///
    /// What that buys: it is self-healing (a device whose keychain was
    /// locked at the time simply moves it later), it costs one extra
    /// lookup only while something is still under the old name, and a
    /// failure to move is never a failure to read.
    ///
    /// "Only while something is still there" is what
    /// [`legacy_names_known_empty`] makes true: without it the extra
    /// lookup would be permanent for every key that has nothing stored.
    ///
    /// Drop the legacy argument after a release or two, the same way an
    /// old migration is deleted. Whatever is left under the old name then
    /// stays there unreachable -- which is the honest trade and the
    /// reason not to rename for taste.
    pub const fn renamed_from(legacy: &'static str, prefix: &'static str) -> Self {
        Secrets { prefix, legacy: Some(legacy) }
    }

    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    fn legacy_key(&self, name: &str) -> Option<String> {
        self.legacy.map(|legacy| format!("{legacy}{name}"))
    }

    /// Reads a secret on behalf of something running by itself.
    pub async fn load(&self, name: &str) -> Result<Option<String>, String> {
        self.load_as(name, Intent::Background).await
    }

    /// Reads a secret because the user just asked for it.
    ///
    /// Waits longer and ignores the circuit breaker: a click is the one
    /// signal that "the keychain was unreachable a moment ago" may no
    /// longer be true.
    pub async fn load_interactive(&self, name: &str) -> Result<Option<String>, String> {
        self.load_as(name, Intent::Interactive).await
    }

    /// For the few paths that serve both a click and a background pass
    /// through the same code -- the sync session restore, above all (see
    /// nobo_sync::commands). Everywhere else, say which one it is by
    /// picking [`load`](Self::load) or
    /// [`load_interactive`](Self::load_interactive).
    pub async fn load_as(&self, name: &str, intent: Intent) -> Result<Option<String>, String> {
        let key = self.key(name);

        match join_or_lead(&key) {
            Flight::Follower(mut leader) => match leader.recv().await {
                Ok(shared) => shared.map_err(|e| e.to_string()),
                // The leader was dropped before it could answer (its
                // caller gave up, or it panicked). Nothing was learned, so
                // do the read here rather than reporting a failure that
                // never happened.
                Err(_) => self.read_through(&key, name, intent).await.map_err(|e| e.to_string()),
            },
            Flight::Leader(followers) => {
                let _leading = LeadingRead(key.clone());
                let result = self.read_through(&key, name, intent).await;
                // Fails only when nobody joined, which is the normal case.
                let _ = followers.send(result.clone());
                result.map_err(|e| e.to_string())
            }
        }
    }

    /// The read itself: the current name, then the name this scope used
    /// to have, moving what it finds.
    async fn read_through(&self, key: &str, name: &str, intent: Intent) -> SharedRead {
        let store = store().map_err(SecretError::Unavailable)?;
        if let Some(value) = store.load(key, intent).await? {
            return Ok(Some(value));
        }

        let Some(legacy_key) = self.legacy_key(name) else { return Ok(None) };
        if legacy_name_is_known_empty(&legacy_key) {
            return Ok(None);
        }
        let Some(value) = store.load(&legacy_key, intent).await? else {
            remember_legacy_name_is_empty(&legacy_key);
            return Ok(None);
        };

        // Moving it is best effort on purpose: the value is right here,
        // and a store that refuses the write now gets another chance on
        // the next read. Answering with it either way is what makes this
        // invisible to the caller.
        if let Err(e) = store.save(key, &value, intent).await {
            log::warn!("could not move a secret to its new name: {e}");
            return Ok(Some(value));
        }
        match store.delete(&legacy_key, intent).await {
            Ok(()) => remember_legacy_name_is_empty(&legacy_key),
            Err(e) => log::warn!("moved a secret but could not remove the old entry: {e}"),
        }
        Ok(Some(value))
    }

    pub async fn save(&self, name: &str, value: &str) -> Result<(), String> {
        self.save_with(name, value, Intent::Background).await
    }

    /// Stores a secret the user just entered.
    pub async fn save_interactive(&self, name: &str, value: &str) -> Result<(), String> {
        self.save_with(name, value, Intent::Interactive).await
    }

    async fn save_with(&self, name: &str, value: &str, intent: Intent) -> Result<(), String> {
        let store = store()?;
        store.save(&self.key(name), value, intent).await.map_err(|e| e.to_string())?;
        // Otherwise the old entry would sit there forever, holding a
        // password the user has just replaced. Skipped once the old name
        // is known to hold nothing -- which, after one read, it is.
        if let Some(legacy_key) = self.legacy_key(name).filter(|key| !legacy_name_is_known_empty(key)) {
            if store.delete(&legacy_key, intent).await.is_ok() {
                remember_legacy_name_is_empty(&legacy_key);
            }
        }
        Ok(())
    }

    /// Removes it under both names: "make sure this is gone" has to mean
    /// gone, not "gone under the name we currently use".
    pub async fn delete(&self, name: &str) -> Result<(), String> {
        let store = store()?;
        store
            .delete(&self.key(name), Intent::Background)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(legacy_key) = self.legacy_key(name).filter(|key| !legacy_name_is_known_empty(key)) {
            store
                .delete(&legacy_key, Intent::Background)
                .await
                .map_err(|e| e.to_string())?;
            remember_legacy_name_is_empty(&legacy_key);
        }
        Ok(())
    }

    /// Best-effort deletion for callers that cannot await -- the result is
    /// only ever logged, so there is nothing to wait for.
    pub fn delete_detached(&self, name: &str) {
        let key = self.key(name);
        let legacy_key = self.legacy_key(name);
        spawn_detached(async move {
            match store() {
                Ok(store) => {
                    if let Err(e) = store.delete(&key, Intent::Background).await {
                        log::warn!("failed to clear a stored secret: {e}");
                    }
                    if let Some(legacy_key) = legacy_key {
                        let _ = store.delete(&legacy_key, Intent::Background).await;
                    }
                }
                Err(e) => log::warn!("failed to clear a stored secret: {e}"),
            }
        });
    }
}

/// Runs a future nobody will await, without asking the host for a
/// runtime.
///
/// A throwaway current-thread runtime on a short-lived OS thread. This
/// used to defer to `tauri::async_runtime::spawn` when the core still
/// knew what Tauri was; it no longer does, and a deletion is rare enough
/// that the thread costs nothing worth optimising -- which is what makes
/// the seam not worth having.
fn spawn_detached(future: impl std::future::Future<Output = ()> + Send + 'static) {
    let spawned = std::thread::Builder::new().name("nobo-secrets-detached".to_string()).spawn(move || {
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime.block_on(future),
            Err(e) => log::warn!("could not run a detached secret operation: {e}"),
        }
    });
    if let Err(e) = spawned {
        log::warn!("could not run a detached secret operation: {e}");
    }
}

// ---------------------------------------------------------------------
// The OS keychain
// ---------------------------------------------------------------------

/// The blocking calls a platform keychain actually offers.
///
/// What [`Keychain`] wraps: one of these, reached only from its worker
/// thread, one call at a time. Everything above it -- the deadline, the
/// circuit breaker, one read per key -- is the same on every platform,
/// so a host brings only this: the Tauri app answers with the securestore
/// plugin (see `nobo_tauri::secrets`), the Sailfish port with Sailfish
/// Secrets through its C++ side. Both go in through
/// [`Keychain::with_backend`].
///
/// Every call may block for as long as the platform takes -- that is the
/// worker's problem, not the backend's. `Ok(None)` from `load` means
/// "nothing stored"; an error that means "could not reach the store at
/// all" must come back as [`SecretError::Unavailable`], because that is
/// what opens the breaker (see [`Breaker`]).
pub trait KeychainBackend: Send + 'static {
    fn load(&mut self, key: &str) -> Result<Option<String>, SecretError>;
    fn save(&mut self, key: &str, value: &str) -> Result<(), SecretError>;
    fn delete(&mut self, key: &str) -> Result<(), SecretError>;
}

/// What the worker is asked to do.
enum Op {
    Load(String),
    Save(String, String),
    Delete(String),
}

/// What it answers with. `Done` is the write side's "nothing to hand
/// back", kept as a value so one channel carries every operation.
enum Answer {
    Value(Option<String>),
    Done,
}

struct Job {
    op: Op,
    /// When the caller stops caring. Checked before the store is touched,
    /// so a worker coming out of a long stall does not work through a
    /// queue of requests nobody is waiting for any more.
    deadline: Instant,
    reply: oneshot::Sender<Result<Answer, SecretError>>,
}

/// The one thread allowed to touch the platform keychain.
///
/// A dedicated OS thread rather than the async runtime's blocking pool,
/// for two reasons that both come from the same fact -- every backend
/// here is blocking:
///
/// - **At most one stuck thread, ever.** A keychain call that never
///   returns (a locked keyring with no prompter, see
///   `tauri_plugin_securestore::linux`) cannot be cancelled. The old
///   shape spawned one blocking task per access and abandoned it on
///   timeout, so a poll every twenty seconds leaked a thread every twenty
///   seconds until the runtime's pool was gone. Here the worst case is
///   this single thread, and the circuit breaker keeps a queue from
///   forming behind it.
/// - **Nothing needs to be `Sync`.** The Linux backend owns a D-Bus
///   connection that is `Send` and not `Sync`; one owner sidesteps that
///   instead of wrapping it.
///
/// It also serialises every keychain access, which is what the Secret
/// Service wanted all along.
struct Worker {
    /// `Mutex` only to be `Sync` -- the send itself never blocks.
    jobs: Mutex<std::sync::mpsc::Sender<Job>>,
}

impl Worker {
    fn spawn(mut backend: impl KeychainBackend) -> Worker {
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("nobo-keychain".to_string())
            .spawn(move || {
                // Ends when the sender is dropped, which happens when the
                // store is replaced or the app shuts down.
                while let Ok(job) = queue.recv() {
                    // Nobody is waiting any more: the caller timed out or
                    // was cancelled. Touching the store now would cost a
                    // keychain round-trip for an answer with no reader --
                    // and on a locked keyring, another prompt.
                    if job.reply.is_closed() || Instant::now() > job.deadline {
                        continue;
                    }
                    let answer = run(&mut backend, job.op);
                    let _ = job.reply.send(answer);
                }
                log::info!("the keychain worker has nothing left to serve");
            })
            .expect("the keychain worker thread could not be started");
        Worker { jobs: Mutex::new(jobs) }
    }

    async fn submit(&self, op: Op, intent: Intent) -> Result<Answer, SecretError> {
        let (reply, answer) = oneshot::channel();
        let deadline = intent.deadline();

        {
            let jobs = self
                .jobs
                .lock()
                .map_err(|_| SecretError::Unavailable("the keychain worker is unusable after a panic".into()))?;
            jobs.send(Job { op, deadline: Instant::now() + deadline, reply })
                .map_err(|_| SecretError::Unavailable("the keychain worker is no longer running".into()))?;
        }

        match tokio::time::timeout(deadline, answer).await {
            Ok(Ok(result)) => result,
            // The worker dropped the reply: it decided nobody was waiting,
            // or it is gone. Either way this request has no answer.
            Ok(Err(_)) => Err(SecretError::Timeout(deadline.as_secs())),
            Err(_) => Err(SecretError::Timeout(deadline.as_secs())),
        }
    }
}

fn run(backend: &mut impl KeychainBackend, op: Op) -> Result<Answer, SecretError> {
    match op {
        Op::Load(key) => backend.load(&key).map(Answer::Value),
        Op::Save(key, value) => backend.save(&key, &value).map(|()| Answer::Done),
        Op::Delete(key) => backend.delete(&key).map(|()| Answer::Done),
    }
}

/// Stops background callers from queueing behind a store that is not
/// answering.
///
/// Serialising every access is what makes this necessary: without it, ten
/// queued startup reads against a locked keyring would each wait their
/// own deadline in turn. With it, the first one pays and the rest are
/// told immediately.
///
/// Only "the store did not answer" opens it. A backend error is about one
/// request, and `Ok(None)` is not a failure at all -- a device that never
/// logged in must not lock itself out.
#[derive(Default)]
struct Breaker {
    open_until: Mutex<Option<Instant>>,
}

impl Breaker {
    /// The error a background caller gets while the breaker is open, or
    /// `None` to go ahead.
    fn refuses(&self, intent: Intent) -> Option<SecretError> {
        if intent == Intent::Interactive {
            return None;
        }
        let open_until = self.open_until.lock().ok()?;
        match *open_until {
            Some(until) if Instant::now() < until => Some(SecretError::Unavailable(
                crate::t!("der Schlüsselbund hat gerade nicht geantwortet", "the keychain did not respond"),
            )),
            _ => None,
        }
    }

    fn note<T>(&self, result: &Result<T, SecretError>) {
        let Ok(mut open_until) = self.open_until.lock() else { return };
        match result {
            Ok(_) => *open_until = None,
            Err(e) if e.store_did_not_answer() => {
                if open_until.is_none() {
                    log::warn!("the keychain did not answer, pausing background access for {BREAKER_WINDOW:?}");
                }
                *open_until = Some(Instant::now() + BREAKER_WINDOW);
            }
            Err(_) => {}
        }
    }
}

/// The OS keychain, reached through one worker with a deadline.
///
/// Everything about why it is shaped this way is on [`Worker`] and
/// [`Breaker`]; what is left here is the mapping from the trait onto
/// them.
pub struct Keychain {
    worker: Worker,
    breaker: Breaker,
}

impl Keychain {
    /// The worker and breaker around a backend the host supplies.
    pub fn with_backend(backend: impl KeychainBackend) -> Self {
        Keychain { worker: Worker::spawn(backend), breaker: Breaker::default() }
    }

    async fn run(&self, op: Op, intent: Intent) -> Result<Answer, SecretError> {
        if let Some(refused) = self.breaker.refuses(intent) {
            return Err(refused);
        }
        let result = self.worker.submit(op, intent).await;
        self.breaker.note(&result);
        result
    }
}

#[async_trait::async_trait]
impl SecretStore for Keychain {
    fn name(&self) -> &'static str {
        "os-keychain"
    }

    async fn load(&self, key: &str, intent: Intent) -> Result<Option<String>, SecretError> {
        match self.run(Op::Load(key.to_string()), intent).await? {
            Answer::Value(value) => Ok(value),
            Answer::Done => Ok(None),
        }
    }

    async fn save(&self, key: &str, value: &str, intent: Intent) -> Result<(), SecretError> {
        self.run(Op::Save(key.to_string(), value.to_string()), intent).await.map(|_| ())
    }

    async fn delete(&self, key: &str, intent: Intent) -> Result<(), SecretError> {
        self.run(Op::Delete(key.to_string()), intent).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is the prefix plus the name, and nothing else -- it is the
    /// on-disk format, and a test is the only thing that will notice if
    /// someone "tidies" it.
    #[test]
    fn a_scope_keys_its_entries_by_its_prefix() {
        assert_eq!(Secrets::new("caldav_password:").key("acc-1"), "caldav_password:acc-1");
        assert_eq!(Secrets::new("sync_").key("session"), "sync_session");
    }

    /// Two features cannot reach each other's secrets by accident: same
    /// name, different key.
    #[test]
    fn two_scopes_do_not_collide_on_the_same_name() {
        assert_ne!(Secrets::new("a_").key("token"), Secrets::new("b_").key("token"));
    }

    /// The edge case that would be silent: with no store installed, a read
    /// must fail rather than answer "nothing stored" -- which every caller
    /// reads as "this device was never logged in".
    #[tokio::test]
    async fn without_a_store_a_read_is_an_error_not_an_empty_answer() {
        // Nothing installs a store in the test binary, so this is the real
        // uninstalled path.
        let result = Secrets::new("test_").load("whatever").await;

        assert!(result.is_err(), "a missing store must not look like a missing entry");
    }

    /// The message the devcontainer produced, which said only "DBus error"
    /// and left the user with nowhere to go.
    #[test]
    fn a_missing_session_bus_is_explained_rather_than_quoted() {
        let message = SecretError::Unavailable(
            "DBus error : /usr/bin/dbus-launch terminated abnormally without any error message".to_string(),
        )
        .to_string();

        assert!(message.contains("Kein Schlüsselbund verfügbar"), "{message}");
        assert!(message.contains("gnome-keyring"), "{message}");
        assert!(message.contains("dbus-launch"), "the original is kept: {message}");
    }

    #[test]
    fn any_other_failure_is_passed_through() {
        let message = SecretError::Backend("disk full".to_string()).to_string();

        assert!(message.contains("disk full"), "{message}");
        assert!(!message.contains("gnome-keyring"), "{message}");
    }

    /// The distinction the breaker rests on: only "could not reach it"
    /// pauses anything.
    #[test]
    fn only_an_unreachable_store_counts_as_not_answering() {
        assert!(SecretError::Unavailable("weg".into()).store_did_not_answer());
        assert!(SecretError::Timeout(10).store_did_not_answer());
        assert!(!SecretError::Backend("abgelehnt".into()).store_did_not_answer());
    }

    /// An interactive caller is never refused: a click is the one signal
    /// that the keychain may be reachable again.
    #[test]
    fn the_breaker_never_refuses_a_user_who_just_asked() {
        let breaker = Breaker::default();
        breaker.note(&Err::<(), _>(SecretError::Timeout(10)));

        assert!(breaker.refuses(Intent::Background).is_some());
        assert!(breaker.refuses(Intent::Interactive).is_none());
    }

    /// "Nothing stored" is the normal state of a fresh device. If it
    /// opened the breaker, that device would lock itself out of its own
    /// keychain on first launch.
    #[test]
    fn an_empty_answer_does_not_open_the_breaker() {
        let breaker = Breaker::default();

        breaker.note(&Ok::<Option<String>, SecretError>(None));

        assert!(breaker.refuses(Intent::Background).is_none());
    }

    /// A backend that answered with a failure says nothing about whether
    /// it can be reached, so the next caller may try.
    #[test]
    fn a_backend_error_does_not_open_the_breaker() {
        let breaker = Breaker::default();

        breaker.note(&Err::<(), _>(SecretError::Backend("abgelehnt".into())));

        assert!(breaker.refuses(Intent::Background).is_none());
    }

    /// Whatever closed over the store, one success means it answers
    /// again.
    #[test]
    fn a_success_closes_the_breaker() {
        let breaker = Breaker::default();
        breaker.note(&Err::<(), _>(SecretError::Unavailable("weg".into())));

        breaker.note(&Ok::<(), SecretError>(()));

        assert!(breaker.refuses(Intent::Background).is_none());
    }

    /// A person waits longer than a background pass -- that is the whole
    /// difference between the two.
    #[test]
    fn a_waiting_person_gets_the_longer_deadline() {
        assert!(Intent::Interactive.deadline() > Intent::Background.deadline());
    }
}
