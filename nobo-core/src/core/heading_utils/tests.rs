    use super::*;
    use crate::core::models::{
        BulkHeadingUpdate, Heading, HeadingCreatePayload, HeadingTarget, HeadingUpdatePayload, OrgTimestamp,
        ProgressStats,
        ORG_PROPERTIES_END, ORG_PROPERTIES_START, ORG_TAGS_COLUMN,
    };
    use crate::core::org_parser;
    use crate::core::heading_identity;
    use crate::core::task_utils::{
        annotate_progress, find_archivable_project_ranges, subtree_progress,
    };
    use crate::utils::fs_utils;
    use std::collections::HashMap;
    use std::num::NonZeroU32;
    use std::path::Path;

    /// A target carrying only a line number -- no CUSTOM_ID, no title.
    /// Enough for tests about the surrounding batching logic; the identity
    /// guard itself is tested in heading_identity.
    fn at_line(line_no: usize) -> HeadingTarget {
        HeadingTarget { line_no, custom_id: None, title: None }
    }

    fn ts(date: &str, time: Option<&str>) -> OrgTimestamp {
        OrgTimestamp {
            start_date: date.to_string(),
            start_time: time.map(String::from),
            end_date: None,
            end_time: None,
            active: true,
            repeater: None,
            extras: Vec::new(),
            raw: String::new(),
        }
    }

    fn make_heading(line_no: usize, last_line: usize, level: usize, title: &str) -> Heading {
        Heading {
            id: format!("test:{}", line_no),
            file_path: "test.org".to_string(),
            line_no,
            last_line,
            level,
            title: title.to_string(),
            ..Default::default()
        }
    }

    fn heading_with_tags(
        line_no: usize,
        last_line: usize,
        level: usize,
        title: &str,
        tags: Vec<&str>,
    ) -> Heading {
        let mut h = make_heading(line_no, last_line, level, title);
        h.tags = tags.into_iter().map(String::from).collect();
        h
    }

    fn empty_payload() -> HeadingUpdatePayload {
        HeadingUpdatePayload {
            priority: None,
            state: None,
            scheduled: None,
            deadline: None,
            properties: None,
            history: None,
            clocks: None,
            content: None,
            closed: None,
            tags: None,
            title: None,
            effort_minutes: None,
        }
    }

    /* --- build_heading_tree --- */

    #[test]
    fn empty_input_returns_empty_tree() {
        assert!(build_heading_tree(vec![]).is_empty());
    }

    #[test]
    fn flat_same_level_headings_are_all_roots() {
        let headings = vec![make_heading(1, 1, 1, "A"), make_heading(2, 2, 1, "B")];
        let tree = build_heading_tree(headings);
        assert_eq!(tree.len(), 2);
        assert!(tree[0].children.is_empty());
        assert!(tree[1].children.is_empty());
    }

    #[test]
    fn deeper_level_becomes_child_of_previous() {
        let headings = vec![
            make_heading(1, 4, 1, "Parent"),
            make_heading(2, 2, 2, "Child"),
        ];
        let tree = build_heading_tree(headings);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].title, "Parent");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].title, "Child");
    }

    #[test]
    fn multi_level_nesting_builds_correct_hierarchy() {
        let headings = vec![
            make_heading(1, 6, 1, "Project"),
            make_heading(2, 4, 2, "Task"),
            make_heading(3, 3, 3, "Subtask"),
            make_heading(4, 4, 2, "Task2"),
        ];
        let tree = build_heading_tree(headings);

        assert_eq!(tree.len(), 1);
        let project = &tree[0];
        assert_eq!(project.children.len(), 2);
        assert_eq!(project.children[0].title, "Task");
        assert_eq!(project.children[0].children.len(), 1);
        assert_eq!(project.children[0].children[0].title, "Subtask");
        assert_eq!(project.children[1].title, "Task2");
    }

    #[test]
    fn sibling_at_same_level_closes_previous_branch() {
        let headings = vec![
            make_heading(1, 4, 1, "A"),
            make_heading(2, 2, 2, "A.child"),
            make_heading(3, 4, 1, "B"),
        ];
        let tree = build_heading_tree(headings);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].children.len(), 1);
        assert!(tree[1].children.is_empty());
    }

    /* --- projects_tree --- */

    #[test]
    fn filters_only_top_level_nodes_with_project_tag() {
        let headings = vec![
            heading_with_tags(1, 2, 1, "Proj A", vec!["project"]),
            make_heading(2, 2, 2, "Task"),
            make_heading(3, 3, 1, "Not a project"),
        ];
        let result = projects_tree(headings);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Proj A");
    }

    #[test]
    fn project_tag_match_is_case_insensitive() {
        let headings = vec![heading_with_tags(1, 1, 1, "Proj", vec!["PROJECT"])];
        let result = projects_tree(headings);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn child_with_project_tag_is_not_included_as_own_result() {
        // Nur Top-Level-Knoten werden gefiltert, ein Kind mit "project"-Tag
        // taucht nicht separat in der Ergebnisliste auf.
        let headings = vec![
            make_heading(1, 4, 1, "Parent"),
            heading_with_tags(2, 2, 2, "Child", vec!["project"]),
        ];
        let result = projects_tree(headings);
        assert!(result.is_empty());
    }

    #[test]
    fn no_project_tags_returns_empty() {
        let headings = vec![make_heading(1, 1, 1, "Plain")];
        assert!(projects_tree(headings).is_empty());
    }

    /* --- find_subtree_end_line / subtree_line_range --- */

    #[test]
    fn leaf_heading_end_line_is_its_own_last_line() {
        let headings = vec![make_heading(1, 5, 1, "Solo")];
        assert_eq!(find_subtree_end_line(&headings, 0), 5);
    }

    #[test]
    fn end_line_extends_through_deeper_children() {
        let headings = vec![
            make_heading(1, 2, 1, "Parent"),
            make_heading(3, 6, 2, "Child"),
        ];
        assert_eq!(find_subtree_end_line(&headings, 0), 6);
    }

    #[test]
    fn end_line_stops_at_sibling_of_same_level() {
        let headings = vec![
            make_heading(1, 2, 1, "Parent"),
            make_heading(3, 4, 1, "Sibling"),
        ];
        assert_eq!(find_subtree_end_line(&headings, 0), 2);
    }

    #[test]
    fn end_line_stops_at_heading_of_shallower_level() {
        let headings = vec![
            make_heading(1, 4, 2, "Parent"),
            make_heading(5, 6, 1, "Higher level"),
        ];
        assert_eq!(find_subtree_end_line(&headings, 0), 4);
    }

    #[test]
    fn subtree_line_range_returns_start_and_end() {
        let headings = vec![
            make_heading(3, 5, 1, "Parent"),
            make_heading(6, 8, 2, "Child"),
        ];
        assert_eq!(subtree_line_range(&headings, 0), (3, 8));
    }

    /* --- format_new_heading_line --- */

    #[test]
    fn formats_minimal_heading_line() {
        // Parameter None am Ende für progress_stats angehängt
        let line = format_new_heading_line(1, &None, None, "Simple Task", &[], None);
        assert_eq!(line, "* Simple Task");
    }

    #[test]
    fn formats_heading_with_state() {
        let line = format_new_heading_line(2, &Some("TODO".to_string()), None, "Do something", &[], None);
        assert_eq!(line, "** TODO Do something");
    }

    #[test]
    fn formats_heading_with_tags_right_aligned() {
        let tags = vec!["work".to_string(), "urgent".to_string()];
        let line = format_new_heading_line(1, &None, None, "Tagged", &tags, None);
        let expected = format!("* Tagged{}:work:urgent:", " ".repeat(59));
        assert_eq!(line, expected);
        assert_eq!(line.chars().count(), ORG_TAGS_COLUMN);
    }

    #[test]
    fn formats_heading_with_state_and_tags_right_aligned() {
        let tags = vec!["home".to_string()];
        let line = format_new_heading_line(3, &Some("DONE".to_string()), None, "Finished", &tags, None);
        let expected_prefix = "*** DONE Finished";
        assert!(line.starts_with(expected_prefix));
        assert!(line.ends_with(":home:"));
        assert_eq!(line.chars().count(), ORG_TAGS_COLUMN);
    }

    #[test]
    fn falls_back_to_min_gap_when_line_already_too_long() {
        let long_title = "x".repeat(100);
        let tags = vec!["a".to_string()];
        let line = format_new_heading_line(1, &None, None, &long_title, &tags, None);
        assert!(line.ends_with(" :a:"));
        assert!(!line.ends_with("  :a:"));
    }

    // --- NEUE TESTS FÜR PROGRESS STATS ---

    #[test]
    fn formats_heading_with_progress_stats() {
        let stats = ProgressStats {
            done: 1,
            total: NonZeroU32::new(2).unwrap()
        };
        let line = format_new_heading_line(1, &None, None, "Project", &[], Some(stats));
        assert_eq!(line, "* Project [50%]");
    }

    #[test]
    fn formats_heading_with_state_and_progress_stats() {
        let stats = ProgressStats {
            done: 3,
            total: NonZeroU32::new(3).unwrap()
        };
        let line = format_new_heading_line(2, &Some("DONE".to_string()), None, "Project", &[], Some(stats));
        assert_eq!(line, "** DONE Project [100%]");
    }

    #[test]
    fn formats_heading_with_progress_and_tags_aligned() {
        let tags = vec!["proj".to_string()];
        let stats = ProgressStats {
            done: 1,
            total: NonZeroU32::new(4).unwrap()
        };
        // Prüft, ob das Auffüllen (Padding) für die Tags die Länge von [25%] korrekt berücksichtigt
        let line = format_new_heading_line(1, &None, None, "Project", &tags, Some(stats));

        let expected_prefix = "* Project [25%]";
        assert!(line.starts_with(expected_prefix));
        assert!(line.ends_with(":proj:"));
        assert_eq!(line.chars().count(), ORG_TAGS_COLUMN);
    }

    #[test]
    fn formats_heading_fallback_gap_with_progress_and_tags() {
        let long_title = "x".repeat(100);
        let tags = vec!["a".to_string()];
        let stats = ProgressStats {
            done: 1,
            total: NonZeroU32::new(2).unwrap()
        };
        let line = format_new_heading_line(1, &None, None, &long_title, &tags, Some(stats));

        assert!(line.starts_with(&format!("* {} [50%]", long_title)));
        assert!(line.ends_with(" :a:")); // Nur 1 Leerzeichen Mindestabstand
    }
    /* --- build_org_heading_block --- */

    #[test]
    fn block_starts_with_formatted_heading_line() {
        let heading = make_heading(1, 1, 2, "My Task");
        let block = build_org_heading_block(&heading, &empty_payload());
        assert_eq!(block[0], "** My Task");
    }

    #[test]
    fn transitioning_to_done_adds_closed_timestamp() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.state = Some("TODO".to_string());

        let mut payload = empty_payload();
        payload.state = Some("DONE".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l.starts_with("CLOSED: [")));
    }

    #[test]
    fn leaving_done_removes_closed_line() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.state = Some("DONE".to_string());
        heading.closed = Some("2025-01-01 Mi 10:00".to_string());

        let mut payload = empty_payload();
        payload.state = Some("TODO".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(!block.iter().any(|l| l.contains("CLOSED:")));
    }

    #[test]
    fn staying_done_keeps_existing_closed_timestamp() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.state = Some("DONE".to_string());
        heading.closed = Some("2025-01-01 Mi 10:00".to_string());

        let mut payload = empty_payload();
        payload.state = Some("DONE".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == "CLOSED: [2025-01-01 Mi 10:00]"));
    }

    #[test]
    fn scheduled_from_payload_overrides_heading() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.scheduled = Some(ts("2025-01-01", None));

        let mut payload = empty_payload();
        payload.scheduled = Some(Some(ts("2025-02-02", None)));

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l.contains("SCHEDULED: <2025-02-02")));
        assert!(!block.iter().any(|l| l.contains("2025-01-01")));
    }

    #[test]
    fn no_scheduled_produces_no_scheduled_line() {
        let heading = make_heading(1, 1, 1, "Task");
        let block = build_org_heading_block(&heading, &empty_payload());
        assert!(!block.iter().any(|l| l.contains("SCHEDULED:")));
    }

    #[test]
    fn deadline_from_payload_overrides_heading() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.deadline = Some(ts("2025-01-01", None));

        let mut payload = empty_payload();
        payload.deadline = Some(Some(ts("2025-02-02", None)));

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l.contains("DEADLINE: <2025-02-02")));
        assert!(!block.iter().any(|l| l.contains("2025-01-01")));
    }

    #[test]
    fn no_deadline_produces_no_deadline_line() {
        let heading = make_heading(1, 1, 1, "Task");
        let block = build_org_heading_block(&heading, &empty_payload());
        assert!(!block.iter().any(|l| l.contains("DEADLINE:")));
    }

    #[test]
    fn scheduled_and_deadline_share_one_planning_line() {
        let heading = make_heading(1, 1, 1, "Task");
        let mut payload = empty_payload();
        payload.scheduled = Some(Some(ts("2025-03-03", None)));
        payload.deadline = Some(Some(ts("2025-03-10", None)));

        let block = build_org_heading_block(&heading, &payload);
        let planning_line = block
            .iter()
            .find(|l| l.contains("SCHEDULED:"))
            .expect("planning line");
        assert!(planning_line.contains("SCHEDULED: <2025-03-03"));
        assert!(planning_line.contains("DEADLINE: <2025-03-10"));
    }

    #[test]
    fn closed_and_scheduled_share_one_planning_line() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.state = Some("TODO".to_string());

        let mut payload = empty_payload();
        payload.state = Some("DONE".to_string());
        payload.scheduled = Some(Some(ts("2025-03-03", None)));

        let block = build_org_heading_block(&heading, &payload);
        let planning_line = block.iter().find(|l| l.contains("CLOSED:")).unwrap();
        assert!(planning_line.contains("SCHEDULED: <2025-03-03"));
    }

    #[test]
    fn scheduled_with_time_is_written_with_day_name() {
        let heading = make_heading(1, 1, 1, "Task");
        let mut payload = empty_payload();
        payload.scheduled = Some(Some(ts("2026-08-05", Some("14:00"))));

        let block = build_org_heading_block(&heading, &payload);
        assert!(block
            .iter()
            .any(|l| l.contains("SCHEDULED: <2026-08-05 Wed 14:00>")));
    }

    #[test]
    fn properties_are_rendered_sorted_between_property_markers() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading
            .properties
            .insert("ZKEY".to_string(), "z".to_string());
        heading
            .properties
            .insert("AKEY".to_string(), "a".to_string());

        let block = build_org_heading_block(&heading, &empty_payload());

        let start_idx = block
            .iter()
            .position(|l| l == ORG_PROPERTIES_START)
            .unwrap();
        let end_idx = block.iter().position(|l| l == ORG_PROPERTIES_END).unwrap();
        let a_idx = block.iter().position(|l| l == ":AKEY: a").unwrap();
        let z_idx = block.iter().position(|l| l == ":ZKEY: z").unwrap();

        // AKEY/ZKEY still sort correctly around whatever else lands between
        // them alphabetically (UPDATED -- see below) -- not asserting fixed
        // offsets from start_idx since that count now varies with it.
        assert!(start_idx < a_idx && a_idx < z_idx && z_idx < end_idx);
    }

    #[test]
    fn every_update_stamps_an_updated_property() {
        // Every direct write -- even one that changes nothing else -- gets
        // a fresh UPDATED AND LAST_ACTIVITY property (see the two
        // constants above): unconditionally, rather than only when the
        // caller happens to touch `properties` itself.
        let heading = make_heading(1, 1, 1, "Task");
        let block = build_org_heading_block(&heading, &empty_payload());

        assert!(block.iter().any(|l| l == ORG_PROPERTIES_START));
        assert!(block.iter().any(|l| l.starts_with(":UPDATED: ")));
        assert!(block.iter().any(|l| l.starts_with(":LAST_ACTIVITY: ")));
    }

    #[test]
    fn a_new_project_carries_its_category_from_the_create_form() {
        let payload = HeadingCreatePayload {
            file_path: "projects.org".to_string(),
            title: "Projekt".to_string(),
            state: None,
            tags: vec!["project".to_string()],
            parent: None,
            scheduled: None,
            effort_minutes: None,
            content: None,
            category: Some("  Arbeit  ".to_string()),
        };

        let block = build_new_heading_block(&payload, 1);

        assert!(block.iter().any(|l| l == ":CATEGORY: Arbeit"), "{block:?}");
    }

    #[test]
    fn a_blank_category_writes_no_property_at_all() {
        let payload = HeadingCreatePayload {
            file_path: "projects.org".to_string(),
            title: "Projekt".to_string(),
            state: None,
            tags: vec![],
            parent: None,
            scheduled: None,
            effort_minutes: None,
            content: None,
            category: Some("   ".to_string()),
        };

        let block = build_new_heading_block(&payload, 1);

        assert!(!block.iter().any(|l| l.starts_with(":CATEGORY:")), "{block:?}");
    }

    /// The reported bug: two projects touched in the same minute compared
    /// EQUAL, so "most recently active first" left them in document order
    /// and adding a subtask to the lower one visibly did nothing.
    #[test]
    fn activity_stamps_carry_seconds_so_two_edits_in_one_minute_still_order() {
        let heading = make_heading(1, 1, 1, "Task");

        let block = build_org_heading_block(&heading, &empty_payload());

        // The time of day is the last space-separated field:
        // "2026-08-23 Sun 12:34:07" -> hh:mm:ss, three parts, not two.
        let time_of = |line: &str| line.rsplit(' ').next().unwrap().split(':').count();
        let activity = block.iter().find(|l| l.starts_with(":LAST_ACTIVITY: ")).unwrap();
        let updated = block.iter().find(|l| l.starts_with(":UPDATED: ")).unwrap();
        assert_eq!(time_of(activity), 3, "{activity}");
        assert_eq!(time_of(updated), 3, "{updated}");
    }

    /// CLOSED is a real org planning timestamp, which org itself writes to
    /// the minute -- it must NOT pick up the seconds the internal stamps
    /// above now carry.
    #[test]
    fn the_closed_planning_line_stays_minute_precise() {
        let heading = make_heading(1, 1, 1, "Task");
        let payload = HeadingUpdatePayload { state: Some("DONE".to_string()), ..empty_payload() };

        let block = build_org_heading_block(&heading, &payload);

        // "CLOSED: [2026-08-23 Sun 12:34]" -> hh:mm, two parts.
        let closed = block.iter().find(|l| l.starts_with("CLOSED: ")).unwrap();
        assert_eq!(closed.rsplit(' ').next().unwrap().split(':').count(), 2, "{closed}");
    }

    #[test]
    fn touch_last_activity_bumps_activity_but_leaves_updated_alone() {
        // The whole point of splitting LAST_ACTIVITY out from UPDATED: a
        // descendant's edit must NOT look like the ancestor itself was
        // directly touched, or "when did I last actually edit this
        // heading" (UPDATED) would become unanswerable the moment any
        // subtask changed underneath it.
        let mut heading = make_heading(1, 1, 1, "Project");
        heading
            .properties
            .insert("UPDATED".to_string(), "2020-01-01 Wed 00:00".to_string());
        heading.properties.insert(
            "LAST_ACTIVITY".to_string(),
            "2020-01-01 Wed 00:00".to_string(),
        );

        let block = touch_last_activity(&heading);

        assert!(
            block.iter().any(|l| l == ":UPDATED: 2020-01-01 Wed 00:00"),
            "UPDATED left untouched"
        );
        assert!(
            block.iter().any(|l| l.starts_with(":LAST_ACTIVITY: ")
                && l != ":LAST_ACTIVITY: 2020-01-01 Wed 00:00"),
            "LAST_ACTIVITY bumped to now"
        );
    }

    #[test]
    fn properties_from_payload_override_heading_properties() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading
            .properties
            .insert("OLD".to_string(), "value".to_string());

        let mut new_props = HashMap::new();
        new_props.insert("NEW".to_string(), "value2".to_string());

        let mut payload = empty_payload();
        payload.properties = Some(new_props);

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == ":NEW: value2"));
        assert!(!block.iter().any(|l| l.contains("OLD")));
    }

    #[test]
    fn state_change_prepends_history_log_entry() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.state = Some("TODO".to_string());

        let mut payload = empty_payload();
        payload.state = Some("DONE".to_string());

        let block = build_org_heading_block(&heading, &payload);
        let log_line = block.iter().find(|l| l.starts_with("- State")).unwrap();
        assert!(log_line.contains("\"DONE"));
        assert!(log_line.contains("from \"TODO"));
    }

    #[test]
    fn no_state_change_adds_no_history_log_entry() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.state = Some("TODO".to_string());

        let mut payload = empty_payload();
        payload.state = Some("TODO".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(!block.iter().any(|l| l.starts_with("- State")));
    }

    #[test]
    fn first_state_assignment_from_empty_adds_no_history_entry() {
        // old_state ist leer → laut Bedingung "!old_state.is_empty()" kein Log-Eintrag.
        let heading = make_heading(1, 1, 1, "Task");

        let mut payload = empty_payload();
        payload.state = Some("TODO".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(!block.iter().any(|l| l.starts_with("- State")));
    }

    #[test]
    fn existing_history_from_payload_overrides_heading_history() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.history = vec!["- old entry".to_string()];

        let mut payload = empty_payload();
        payload.history = Some(vec!["- new entry".to_string()]);

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == "- new entry"));
        assert!(!block.iter().any(|l| l == "- old entry"));
    }

    #[test]
    fn logbook_drawer_holds_clocks_then_history_after_properties_before_content() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.properties.insert("CUSTOM_ID".to_string(), "abc".to_string());
        heading.clocks = vec!["CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30".to_string()];
        heading.history = vec!["- State \"DONE\"        from \"TODO\"        [2026-08-17 Mon 14:30]".to_string()];
        heading.content = "Body text.".to_string();

        let block = build_org_heading_block_unaltered(&heading);

        let properties_start = block.iter().position(|l| l == ":PROPERTIES:").unwrap();
        let properties_end = block.iter().position(|l| l == ":END:").unwrap();
        let logbook_start = block.iter().position(|l| l == ":LOGBOOK:").unwrap();
        let clock_pos = block.iter().position(|l| l.starts_with("CLOCK:")).unwrap();
        let history_pos = block.iter().position(|l| l.starts_with("- State")).unwrap();
        let logbook_end = block[logbook_start + 1..].iter().position(|l| l == ":END:").map(|i| i + logbook_start + 1).unwrap();
        let content_pos = block.iter().position(|l| l == "Body text.").unwrap();

        assert!(properties_start < properties_end, "PROPERTIES opens before it closes");
        assert!(properties_end < logbook_start, "LOGBOOK comes after PROPERTIES");
        assert!(clock_pos < history_pos, "clocks come before history inside LOGBOOK");
        assert!(history_pos < logbook_end, "history is inside the LOGBOOK drawer");
        assert!(logbook_end < content_pos, "LOGBOOK closes before the body text");
    }

    #[test]
    fn no_logbook_drawer_when_both_clocks_and_history_are_empty() {
        let heading = make_heading(1, 1, 1, "Task");
        let block = build_org_heading_block_unaltered(&heading);
        assert!(!block.iter().any(|l| l == ":LOGBOOK:"));
    }

    #[test]
    fn logbook_drawer_present_with_only_clocks_and_no_history() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.clocks = vec!["CLOCK: [2026-08-17 Mon 18:45]".to_string()];
        let block = build_org_heading_block_unaltered(&heading);
        assert!(block.iter().any(|l| l == ":LOGBOOK:"));
        assert!(block.iter().any(|l| l == "CLOCK: [2026-08-17 Mon 18:45]"));
    }

    #[test]
    fn roundtrips_the_users_own_full_example_heading_byte_exact_structure() {
        // The exact example the user gave as the compatibility spec:
        // PROPERTIES (with CATEGORY) -> LOGBOOK (two CLOCK entries,
        // newest first) -> plain body text. Parsing it, then rewriting it
        // completely unaltered, must reproduce the same structural order
        // (PROPERTIES -> LOGBOOK -> body) and the same content -- the
        // actual Emacs-compatibility criterion, not just "parses without
        // crashing".
        let original = concat!(
            "* NEXT Login Screen bauen\n",
            "  :PROPERTIES:\n",
            "  :CATEGORY: App-Dev\n",
            "  :END:\n",
            "  :LOGBOOK:\n",
            "  CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30\n",
            "  CLOCK: [2026-08-16 Sun 09:00]--[2026-08-16 Sun 09:45] =>  0:45\n",
            "  :END:\n",
            "  Hier steht dann die eigentliche Beschreibung oder Notiz zur Aufgabe.\n",
        );

        let path = std::path::Path::new("test.org");
        let flat = org_parser::parse_headings_from_lines(path, original.lines());
        assert_eq!(flat.len(), 1);
        let heading = &flat[0];

        assert_eq!(heading.properties.get("CATEGORY"), Some(&"App-Dev".to_string()));
        // Leading whitespace is preserved verbatim from the source (same
        // convention `history` already has -- see ORG_STATE_LOG's own
        // `line.trim_end()`, not `line.trim()`), so this indented example
        // parses with its original indentation intact; the actual
        // Emacs-format compatibility criterion (byte-exact CLOCK line
        // syntax itself) is exhaustively covered by clock_utils.rs's own
        // parse/format round-trip tests, not by whitespace here.
        let trimmed_clocks: Vec<&str> = heading.clocks.iter().map(|s| s.trim()).collect();
        assert_eq!(trimmed_clocks, vec![
            "CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30",
            "CLOCK: [2026-08-16 Sun 09:00]--[2026-08-16 Sun 09:45] =>  0:45",
        ]);
        assert_eq!(heading.content.trim(), "Hier steht dann die eigentliche Beschreibung oder Notiz zur Aufgabe.");

        let rewritten = build_org_heading_block_unaltered(heading);

        let properties_pos = rewritten.iter().position(|l| l == ":PROPERTIES:").unwrap();
        let logbook_pos = rewritten.iter().position(|l| l == ":LOGBOOK:").unwrap();
        let clock1_pos = rewritten.iter().position(|l| l.trim() == "CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30").unwrap();
        let clock2_pos = rewritten.iter().position(|l| l.trim() == "CLOCK: [2026-08-16 Sun 09:00]--[2026-08-16 Sun 09:45] =>  0:45").unwrap();
        let body_pos = rewritten.iter().position(|l| l.trim() == "Hier steht dann die eigentliche Beschreibung oder Notiz zur Aufgabe.").unwrap();

        assert!(properties_pos < logbook_pos, "PROPERTIES before LOGBOOK");
        assert!(logbook_pos < clock1_pos, "LOGBOOK opens before its own CLOCK lines");
        assert!(clock1_pos < clock2_pos, "clock order preserved (newest first)");
        assert!(clock2_pos < body_pos, "LOGBOOK/its content comes before the body text");

        // A second parse+rewrite pass (simulating a subsequent edit/sync)
        // must be stable -- no drift, no data loss.
        let flat2 = org_parser::parse_headings_from_lines(path, rewritten.join("\n").lines());
        assert_eq!(flat2[0].clocks, heading.clocks);
        assert_eq!(flat2[0].properties.get("CATEGORY"), Some(&"App-Dev".to_string()));
        assert_eq!(flat2[0].content.trim(), heading.content.trim());
    }

    #[test]
    fn content_from_payload_overrides_heading_content_and_is_appended() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading.content = "old content".to_string();

        let mut payload = empty_payload();
        payload.content = Some("line one\nline two".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == "line one"));
        assert!(block.iter().any(|l| l == "line two"));
        assert!(!block.iter().any(|l| l == "old content"));
    }

    /// A parse -> build -> splice -> reparse round trip through a temp
    /// file, exactly what happens on a real save+refetch of ONE heading
    /// (`target_idx` into the flat list) -- proves content survives a FULL
    /// cycle, including its effect on OTHER headings in the same file, not
    /// just that build_org_heading_block's own output looks right for that
    /// heading in isolation.
    fn roundtrip_one(initial: &str, target_idx: usize, new_content: &str) -> Vec<Heading> {
        let file = tempfile::NamedTempFile::with_suffix(".org").unwrap();
        std::fs::write(file.path(), initial).unwrap();
        let flat = org_parser::extract_headings_from_file(file.path());
        let heading = &flat[target_idx];

        let mut payload = empty_payload();
        payload.content = Some(new_content.to_string());
        let block = build_org_heading_block(heading, &payload);

        fs_utils::splice_lines(file.path(), heading.line_no - 1, heading.last_line, block).unwrap();
        org_parser::extract_headings_from_file(file.path())
    }

    fn roundtrip(initial: &str, new_content: &str) -> Vec<Heading> {
        roundtrip_one(initial, 0, new_content)
    }

    #[test]
    fn description_a_bare_asterisk_line_does_not_spawn_a_new_heading_or_swallow_siblings() {
        // Exact reported bug: a user's description contains plain prose
        // that happens to include lines starting with '*' (e.g. pasted
        // signature-style text). Written back out naively, the next parse
        // would misread "* Test blabla" as a new heading, silently
        // re-parenting/erasing whatever came after it in the file. Real
        // fixture matching the report: a project with two subtasks, the
        // FIRST subtask's description contains bare '*'/'**' lines, the
        // SECOND subtask must survive completely untouched.
        let initial = concat!(
            "* TODO Parent :project:\n",
            "** DONE Unteraufgabe hinzufuegen\n",
            "Normal description.\n",
            "** NEXT Test\n",
            "Also normal.\n",
        );
        let dangerous = "Test blabla unter\nNEXT Test blabla und * Test blabla, das darf nichts kaputt machen\n** Wird nicht als eigene aktion angezeigt\n* Auch nicht als eigenes projekt";

        let flat = roundtrip_one(initial, 1, dangerous);

        assert_eq!(flat.len(), 3, "still exactly Parent + 2 subtasks, nothing spawned or swallowed");
        assert_eq!(flat[0].title, "Parent");
        assert_eq!(flat[1].title, "Unteraufgabe hinzufuegen");
        assert_eq!(flat[2].title, "Test");
        assert_eq!(flat[2].content.trim(), "Also normal.", "the sibling AFTER the dangerous description must survive untouched");
        assert!(flat[1].content.contains("Test blabla"));
        // The user's own SHORT form comes back exactly as typed -- on disk
        // it's the demoted long form (see apply_content_heading_offset),
        // stripped again on read (see org_parser::strip_content_heading_offset).
        assert!(flat[1].content.contains("* Auch nicht als eigenes projekt"), "the typed short form survives round-trip, unchanged");
        assert!(flat[1].content.contains("** Wird nicht als eigene aktion angezeigt"));
    }

    #[test]
    fn description_typed_star_line_is_demoted_on_disk_by_the_hosting_headings_own_level() {
        // Formula from the user's own spec: task's own level + typed level
        // = saved level. A level-2 subtask ("** ..."), typed "* Notiz" (1
        // star) -> saved as 2+1 = 3 stars on disk.
        let heading = make_heading(1, 1, 2, "Task");
        let mut payload = empty_payload();
        payload.content = Some("* Notiz".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == "*** Notiz"), "expected '*** Notiz' in {:?}", block);
    }

    #[test]
    fn description_star_run_without_a_following_space_is_never_demoted_just_indented() {
        // "*Achtung*" looks like inline emphasis, not a heading attempt --
        // real Org headings require a space after the stars. Demoting it
        // would glue MORE stars directly onto the word with no space,
        // which is both ugly and still ambiguous. A single leading space
        // is enough on its own: real Org headings require column 0, so an
        // indented line can never be misread as one, regardless of level.
        let heading = make_heading(1, 1, 3, "Task");
        let mut payload = empty_payload();
        payload.content = Some("*Achtung* das ist wichtig".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == " *Achtung* das ist wichtig"), "expected a single leading space, not demotion, in {:?}", block);
    }

    #[test]
    fn description_plain_prose_without_any_leading_star_is_untouched() {
        let heading = make_heading(1, 1, 2, "Task");
        let mut payload = empty_payload();
        payload.content = Some("Just plain prose.\nNo special syntax here.".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == "Just plain prose."));
        assert!(block.iter().any(|l| l == "No special syntax here."));
    }

    #[test]
    fn description_typing_a_real_todo_keyword_creates_a_genuine_correctly_nested_subtask() {
        // The deliberate flip side of the safety net: a typed line that
        // DOES carry a real TODO keyword is meant to become a real
        // subtask, not just a note -- and the same demotion formula is
        // what makes it land at the CORRECT depth automatically. A
        // level-2 task, typed "* TODO Datenbank" (1 star) -> demoted to
        // 2+1=3 stars -> "*** TODO Datenbank" -- a real level-3 heading
        // with a state, one level under the level-2 host.
        let initial = "* TODO Parent :project:\n** TODO Login\nOld.\n";
        let flat = roundtrip_one(initial, 1, "* TODO Datenbank");

        assert_eq!(flat.len(), 3, "Parent + Login + the new real subtask Datenbank");
        assert_eq!(flat[2].title, "Datenbank");
        assert_eq!(flat[2].state, Some("TODO".to_string()));
        assert_eq!(flat[2].level, 3);
    }

    #[test]
    fn description_a_note_typed_after_a_real_subtask_in_the_same_block_nests_under_that_subtask_not_the_original_host() {
        // A note typed below a real heading in the same description ends
        // up textually after it, so org's rule makes it that new
        // subtask's content, not the host's. Demoting by the host's flat
        // level lands it too shallow to be absorbed at all -- it has to
        // land deeper than the new subtask's own level.
        let initial = "* TODO Parent :project:\n** TODO Login\nOld.\n";
        let flat = roundtrip_one(initial, 1, "* TODO Blabla\n* Das ist ein Test");

        assert_eq!(flat.len(), 3, "Parent + Login + the new real subtask Blabla");
        assert_eq!(flat[2].title, "Blabla");
        assert_eq!(flat[2].level, 3, "Blabla nests one level under Login (2+1)");
        assert_eq!(
            flat[2].content.trim(), "* Das ist ein Test",
            "the note survives as Blabla's OWN content, in its original short form"
        );
        assert!(!flat[1].content.contains("Das ist ein Test"), "must NOT still be attached to Login (the original host)");
    }

    #[test]
    fn description_two_real_subtasks_typed_at_the_same_level_become_siblings_not_nested() {
        // Two "* TODO ..." lines typed at the same (1-star) depth in one
        // description must become SIBLINGS once pulled out as real
        // subtasks -- not have the second one nest INSIDE the first just
        // because it comes later in the text.
        let initial = "* TODO Parent :project:\n** TODO Login\nOld.\n";
        let flat = roundtrip_one(initial, 1, "* TODO Erste\n* TODO Zweite");

        assert_eq!(flat.len(), 4, "Parent + Login + Erste + Zweite");
        assert_eq!(flat[2].title, "Erste");
        assert_eq!(flat[3].title, "Zweite");
        assert_eq!(flat[2].level, flat[3].level, "both land at the same level -- true siblings");
        assert_eq!(flat[2].level, 3);
    }

    #[test]
    fn description_a_note_after_each_of_two_sibling_subtasks_attaches_to_the_right_one() {
        let initial = "* TODO Parent :project:\n** TODO Login\nOld.\n";
        let flat = roundtrip_one(
            initial,
            1,
            "* TODO Erste\n* Notiz zu Erste\n* TODO Zweite\n* Notiz zu Zweite",
        );

        assert_eq!(flat.len(), 4);
        assert_eq!(flat[2].title, "Erste");
        assert_eq!(flat[2].content.trim(), "* Notiz zu Erste");
        assert_eq!(flat[3].title, "Zweite");
        assert_eq!(flat[3].content.trim(), "* Notiz zu Zweite");
    }

    #[test]
    fn description_a_more_deeply_typed_real_subtask_nests_as_a_child_not_a_sibling() {
        // "** TODO Nested" (2 typed stars) after "* TODO Blabla" (1 typed
        // star) must become Blabla's CHILD -- one level deeper than
        // Blabla, matching the RELATIVE star-depth difference the user
        // actually typed (2 - 1 = 1 extra level), not just "host_level +
        // typed_level" applied flatly (which would overshoot).
        let initial = "* TODO Parent :project:\n** TODO Login\nOld.\n";
        let flat = roundtrip_one(initial, 1, "* TODO Blabla\n** TODO Nested");

        assert_eq!(flat.len(), 4, "Parent + Login + Blabla + Nested");
        assert_eq!(flat[2].title, "Blabla");
        assert_eq!(flat[2].level, 3);
        assert_eq!(flat[3].title, "Nested");
        assert_eq!(flat[3].level, 4, "exactly one level deeper than Blabla, not two");
    }

    #[test]
    fn description_multiple_paragraphs_around_a_typed_heading_line_all_survive() {
        let flat = roundtrip(
            "* TODO Task\nOld.\n",
            "First paragraph.\n\n* Notiz\n\nLast paragraph, must survive.",
        );
        assert_eq!(flat.len(), 1);
        assert!(flat[0].content.contains("First paragraph."));
        assert!(flat[0].content.contains("* Notiz"));
        assert!(flat[0].content.contains("Last paragraph, must survive."));
    }

    #[test]
    fn description_a_multi_line_pasted_block_is_protected_exactly_like_typed_text() {
        // The mechanism operates on whatever content string it is handed
        // and has no notion of typing versus pasting -- the frontend
        // always hands over the full current text, never a diff -- so a
        // pasted chunk must come through exactly like typed text.
        let pasted_block = concat!(
            "Copied from my other notes app:\n",
            "* Erste Idee\n",
            "** Detail dazu\n",
            "  - some sub-bullet, not a heading (indented)\n",
            "\n",
            "* Zweite Idee\n",
            "Ende.",
        );
        let flat = roundtrip("* TODO Task\nOld.\n", pasted_block);

        assert_eq!(flat.len(), 1, "the pasted block's heading-looking lines must not spawn any real headings");
        assert!(flat[0].content.contains("Copied from my other notes app:"));
        assert!(flat[0].content.contains("* Erste Idee"), "typed short form survives, byte-identical to what was pasted");
        assert!(flat[0].content.contains("** Detail dazu"));
        assert!(flat[0].content.contains("some sub-bullet, not a heading (indented)"));
        assert!(flat[0].content.contains("* Zweite Idee"));
        assert!(flat[0].content.contains("Ende."), "content after the LAST pasted heading-looking line must not be dropped");
    }

    #[test]
    fn a_stray_non_project_heading_between_real_projects_is_absorbed_as_a_sub_note_not_left_standing() {
        // Direct consequence of the reintroduced structure rule (see
        // org_parser.rs: a '*'-line is only real structure with a TODO
        // state or a :project: tag): a bare, state-less, :project:-less
        // heading between two real projects (e.g. hand-authored, or
        // leftover from some other edit) is absorbed as a sub-note of
        // whichever real heading precedes it, rather than becoming its
        // own phantom project -- and a LATER real project is completely
        // unaffected either way.
        let file = tempfile::NamedTempFile::with_suffix(".org").unwrap();
        std::fs::write(file.path(), concat!(
            "* Project One :project:\n",
            "** TODO Subtask A\n",
            "* Just a heading, not a project\n",
            "Some notes under it.\n",
            "* Project Two :project:\n",
            "** NEXT Subtask B\n",
        )).unwrap();
        let flat = org_parser::extract_headings_from_file(file.path());

        assert_eq!(flat.len(), 4, "Project One, Subtask A (now absorbing the stray heading + its notes), Project Two, Subtask B");
        assert_eq!(flat[0].title, "Project One");
        assert!(flat[0].tags.contains(&"project".to_string()));
        assert_eq!(flat[1].title, "Subtask A");
        assert!(flat[1].content.contains("Just a heading, not a project"));
        assert!(flat[1].content.contains("Some notes under it."));
        assert_eq!(flat[2].title, "Project Two");
        assert!(flat[2].tags.contains(&"project".to_string()));
        assert_eq!(flat[3].title, "Subtask B");
    }

    #[test]
    fn blank_content_produces_no_content_lines() {
        let heading = make_heading(1, 1, 1, "Task");
        let mut payload = empty_payload();
        payload.content = Some("   ".to_string());

        let block = build_org_heading_block(&heading, &payload);
        // Nothing from the blank content itself -- just the heading line
        // plus the always-present :PROPERTIES: drawer (UPDATED +
        // LAST_ACTIVITY stamps; no CUSTOM_ID here -- an existing heading
        // being edited never gets one silently, see
        // build_org_heading_block's own doc comment).
        assert!(!block.iter().any(|l| l.contains("   ")));
        assert_eq!(block.len(), 5);
    }

    #[test]
    fn editing_an_existing_heading_never_silently_adds_a_custom_id() {
        // An existing heading must only get a CUSTOM_ID via the explicit
        // consented backfill, never as a side effect of an unrelated
        // edit -- which would bypass the ask-first prompt entirely.
        let heading = make_heading(1, 1, 1, "Task");
        let mut payload = empty_payload();
        payload.title = Some("Renamed Task".to_string());

        let block = build_org_heading_block(&heading, &payload);
        assert!(!block.iter().any(|l| l.contains("CUSTOM_ID")), "{block:?}");
    }

    #[test]
    fn touching_ancestors_never_silently_adds_a_custom_id_either() {
        let heading = make_heading(1, 1, 1, "Project");
        let block = touch_last_activity(&heading);
        assert!(!block.iter().any(|l| l.contains("CUSTOM_ID")), "{block:?}");
    }

    #[test]
    fn stamp_custom_id_only_is_the_sole_path_that_adds_one_to_an_existing_heading() {
        let heading = make_heading(1, 1, 1, "Task");
        let block = stamp_custom_id_only(&heading);
        assert!(block.iter().any(|l| l.contains("CUSTOM_ID")), "{block:?}");
        // And, unlike a real edit/ancestor touch, doesn't stamp UPDATED or
        // LAST_ACTIVITY either -- this is mechanical maintenance, not
        // activity on the heading (see its own doc comment).
        assert!(
            !block
                .iter()
                .any(|l| l.contains("UPDATED") || l.contains("LAST_ACTIVITY")),
            "{block:?}"
        );
    }

    #[test]
    fn stamp_custom_id_only_never_overwrites_an_existing_id() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading
            .properties
            .insert("CUSTOM_ID".to_string(), "keep-me".to_string());
        let block = stamp_custom_id_only(&heading);
        assert!(
            block.iter().any(|l| l == ":CUSTOM_ID: keep-me"),
            "{block:?}"
        );
    }

    #[test]
    fn effort_from_payload_is_written_canonically() {
        let heading = make_heading(1, 1, 1, "Task");
        let mut payload = empty_payload();
        payload.effort_minutes = Some(Some(90));

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == ":Effort: 1:30"));
    }

    #[test]
    fn effort_none_removes_property() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading
            .properties
            .insert("Effort".to_string(), "1:30".to_string());

        let mut payload = empty_payload();
        payload.effort_minutes = Some(None);

        let block = build_org_heading_block(&heading, &payload);
        assert!(!block.iter().any(|l| l.contains("Effort")));
    }

    #[test]
    fn effort_update_preserves_existing_key_casing() {
        let mut heading = make_heading(1, 1, 1, "Task");
        heading
            .properties
            .insert("EFFORT".to_string(), "1:00".to_string());

        let mut payload = empty_payload();
        payload.effort_minutes = Some(Some(30));

        let block = build_org_heading_block(&heading, &payload);
        assert!(block.iter().any(|l| l == ":EFFORT: 0:30"));
        assert!(!block.iter().any(|l| l == ":Effort: 0:30"));
    }

    #[test]
    fn progress_is_none_without_children() {
        let flat = vec![make_heading(1, 1, 1, "Solo")];
        assert_eq!(subtree_progress(&flat, 0), None);
    }

    #[test]
    fn progress_counts_only_stateful_descendants() {
        let mut flat = vec![
            make_heading(1, 4, 1, "P"),
            make_heading(2, 2, 2, "Notiz"), // kein State
            make_heading(3, 3, 2, "A"),
            make_heading(4, 4, 2, "B"),
        ];
        flat[2].state = Some("DONE".into());
        flat[3].state = Some("TODO".into());

        assert_eq!(
            subtree_progress(&flat, 0),
            Some(ProgressStats {
                done: 1,
                total: NonZeroU32::new(2).unwrap(),
            })
        );
    }

    #[test]
    fn progress_treats_obe_and_wont_do_as_finished() {
        let mut flat = vec![
            make_heading(1, 3, 1, "P"),
            make_heading(2, 2, 2, "A"),
            make_heading(3, 3, 2, "B"),
        ];
        flat[1].state = Some("OBE".into());
        flat[2].state = Some("WONT-DO".into());

        assert_eq!(
            subtree_progress(&flat, 0),
            Some(ProgressStats {
                done: 2,
                total: NonZeroU32::new(2).unwrap(),
            })
        );
    }

    #[test]
    fn archivable_only_includes_completed_projects_with_tag() {
        let mut flat = vec![
            heading_with_tags(1, 2, 1, "Fertig", vec!["project"]),
            make_heading(2, 2, 2, "A"),
            heading_with_tags(3, 4, 1, "Offen", vec!["project"]),
            make_heading(4, 4, 2, "B"),
            heading_with_tags(5, 5, 1, "Leer", vec!["project"]),
        ];
        flat[1].state = Some("DONE".into());
        flat[3].state = Some("TODO".into());
        annotate_progress(&mut flat);

        let ranges = find_archivable_project_ranges(&flat);
        assert_eq!(ranges, vec![(1, 2)]);
    }

    #[test]
    fn archivable_ignores_untagged_top_level_headings() {
        let mut flat = vec![
            make_heading(1, 2, 1, "Kein Projekt"),
            make_heading(2, 2, 2, "A"),
        ];
        flat[1].state = Some("DONE".into());

        assert!(find_archivable_project_ranges(&flat).is_empty());
    }

    /* --- progress cookie staleness (touch_ancestors / apply_bulk_update / apply_bulk_delete) --- */

    /// A project's `[N%]` cookie must reflect the edit that triggered the
    /// touch, not the pre-edit parse the ancestors were captured from.
    /// Follows the same sequence an update does: capture ancestors, apply
    /// the edit, then touch.
    #[test]
    fn touch_ancestors_writes_a_fresh_progress_cookie_reflecting_the_edit_just_applied() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        std::fs::write(&path, concat!(
            "* Project [50%]                                                    :project:\n",
            "** DONE Subtask A\n",
            "** TODO Subtask B\n",
        )).unwrap();

        let flat = org_parser::parse_headings_from_lines(&path, std::fs::read_to_string(&path).unwrap().lines());
        let idx = flat.iter().position(|h| h.title == "Subtask B").unwrap();
        let ancestors = ancestors_of(&flat, idx);
        assert_eq!(
            ancestors[0].progress_stats,
            Some(ProgressStats { done: 1, total: NonZeroU32::new(2).unwrap() }),
            "sanity: the captured pre-edit progress is still 50%"
        );

        let payload = HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() };
        let mut edit = crate::core::heading_utils::open_edit(&path, &at_line(flat[idx].line_no), &crate::core::errors::subject::heading()).unwrap();
        edit.write(&payload).unwrap();
        edit.touch_ancestors().unwrap();
        edit.commit().unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        let project_line = result.lines().next().unwrap();
        assert!(project_line.contains("[100%]"), "reflects BOTH children now being done: {project_line}");
        assert!(!project_line.contains("[50%]"), "must not still show the stale pre-edit value: {project_line}");
    }

    /// Same staleness bug, for the batched bulk-update path -- an
    /// ancestor's touch there happens against the SAME pre-batch FLAT with
    /// no reparse in between (that's the whole point of it being one
    /// pass), so it needs the state_overlay fix instead of a reparse.
    #[test]
    fn apply_bulk_update_ancestor_touch_reflects_the_batchs_own_pending_state_changes() {
        let text = "* Project                                                          :project:\n** TODO Subtask A\n** TODO Subtask B\n";
        let flat = org_parser::parse_headings_from_lines(Path::new("projects.org"), text.lines());
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        let updates = vec![
            BulkHeadingUpdate { target: at_line(line_of("Subtask A")), payload: HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() } },
            BulkHeadingUpdate { target: at_line(line_of("Subtask B")), payload: HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() } },
        ];

        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        apply_bulk_update(&flat, &updates, &mut lines).unwrap();

        let project_line = lines.iter().find(|l| l.contains("Project")).unwrap();
        assert!(project_line.contains("[100%]"), "both children marked DONE in the same batch -- the project's cookie must reflect that, not the pre-batch 0%: {project_line}");
    }

    /// Same staleness bug, for the batched bulk-delete path -- an
    /// ancestor's touch there must exclude descendants the SAME batch is
    /// about to remove from its progress count, not just what FLAT (the
    /// pre-batch parse) still shows.
    #[test]
    fn apply_bulk_delete_ancestor_touch_excludes_the_deleted_descendants_from_progress() {
        let text = "* Project                                                          :project:\n** DONE Subtask A\n** TODO Subtask B\n** TODO Subtask C\n";
        let flat = org_parser::parse_headings_from_lines(Path::new("projects.org"), text.lines());
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;
        assert_eq!(
            flat[0].progress_stats,
            Some(ProgressStats { done: 1, total: NonZeroU32::new(3).unwrap() }),
            "sanity: pre-delete progress is 1/3"
        );

        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        apply_bulk_delete(&flat, &[at_line(line_of("Subtask C"))], &mut lines).unwrap();

        let project_line = lines.iter().find(|l| l.contains("Project")).unwrap();
        assert!(project_line.contains("[50%]"), "C removed -- remaining count is 1 done / 2 total, not the stale pre-delete 1/3: {project_line}");
    }

    /// Reported bug: a description written in the "Neue Unteraufgabe"
    /// form was gone again later. `build_new_heading_block` wrote the
    /// content verbatim while every EDIT of the same field demotes it
    /// (see apply_content_heading_offset) -- so a typed "* TODO ..." line
    /// escaped as a real level-1 heading. The new subtask lost its
    /// description from that line on, and every heading after the
    /// insertion point re-parented underneath the escapee.
    #[test]
    fn a_new_subtasks_description_is_demoted_exactly_like_an_edited_one() {
        let file = tempfile::NamedTempFile::with_suffix(".org").unwrap();
        let initial = concat!(
            "* TODO Projekt :project:\n",
            "** TODO Gruppe\n",
            "*** TODO Bestehende Unteraufgabe\n",
            "Alter Text.\n",
            "** TODO Zweite Gruppe\n",
            "Text der zweiten Gruppe.\n",
        );
        std::fs::write(file.path(), initial).unwrap();

        let flat = org_parser::extract_headings_from_file(file.path());
        let parent_idx = flat.iter().position(|h| h.title == "Gruppe").unwrap();
        let parent = &flat[parent_idx];

        let payload = HeadingCreatePayload {
            file_path: file.path().to_string_lossy().into_owned(),
            title: "Neue Unteraufgabe".to_string(),
            state: Some("TODO".to_string()),
            tags: vec![],
            parent: Some(HeadingTarget { line_no: parent.line_no, custom_id: None, title: Some(parent.title.clone()) }),
            scheduled: None,
            effort_minutes: None,
            content: Some("Zeile eins\n* Eine Notiz\n* TODO Ein echter Schritt".to_string()),
            category: None,
        };

        let block = build_new_heading_block(&payload, parent.level + 1);
        let insert_after = find_subtree_end_line(&flat, parent_idx);
        fs_utils::insert_lines_after(file.path(), insert_after, block).unwrap();

        let reparsed = org_parser::extract_headings_from_file(file.path());
        let titles: Vec<&str> = reparsed.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Projekt", "Gruppe", "Bestehende Unteraufgabe", "Neue Unteraufgabe", "Ein echter Schritt", "Zweite Gruppe"],
            "nothing may be swallowed or re-ordered"
        );

        let created = reparsed.iter().find(|h| h.title == "Neue Unteraufgabe").unwrap();
        assert_eq!(created.content, "Zeile eins\n* Eine Notiz", "the typed short form comes back exactly as typed");

        // The typed real subtask nests UNDER the new task (3 + 1), and the
        // untouched sibling keeps its own place in the project, rather
        // than becoming a child of an escaped level-1 heading.
        let real_child = reparsed.iter().find(|h| h.title == "Ein echter Schritt").unwrap();
        assert_eq!(real_child.level, 4);
        let sibling = reparsed.iter().find(|h| h.title == "Zweite Gruppe").unwrap();
        assert_eq!(sibling.level, 2);
        assert_eq!(sibling.content, "Text der zweiten Gruppe.");
    }

    /// The create path and the edit path must produce byte-identical
    /// content lines for the same typed description -- the asymmetry above
    /// is exactly what let one of them corrupt the file.
    #[test]
    fn create_and_edit_write_the_same_description_lines() {
        let typed = "Prosa\n* Notiz\n** Tiefer\n*Achtung* fett\n* TODO Schritt";

        let payload = HeadingCreatePayload {
            file_path: "projects.org".to_string(),
            title: "Task".to_string(),
            state: Some("TODO".to_string()),
            tags: vec![],
            parent: Some(HeadingTarget { line_no: 1, custom_id: None, title: None }),
            scheduled: None,
            effort_minutes: None,
            content: Some(typed.to_string()),
            category: None,
        };
        let created = build_new_heading_block(&payload, 3);

        let heading = make_heading(1, 1, 3, "Task");
        let mut edit = empty_payload();
        edit.content = Some(typed.to_string());
        let edited = build_org_heading_block(&heading, &edit);

        let body_of = |block: &[String]| -> Vec<String> {
            let end = block.iter().rposition(|l| l == ORG_PROPERTIES_END).unwrap();
            block[end + 1..].to_vec()
        };
        assert_eq!(body_of(&created), body_of(&edited));
    }

    /// The rule the vault's forward compatibility rests on: a property
    /// this build knows nothing about -- written by a NEWER build, or by
    /// the user's own Emacs -- survives an edit made here untouched.
    ///
    /// Without this, two devices on two app versions would strip each
    /// other's data on every write, and the only way out would be
    /// migrating the vault (which cannot work while both versions are in
    /// use). With it, a new property is simply added and old builds carry
    /// it along.
    #[test]
    fn a_property_this_build_does_not_know_survives_an_edit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.org");
        std::fs::write(
            &file,
            "* TODO Etwas tun\n:PROPERTIES:\n:CUSTOM_ID: h:abc\n:AUS_DER_ZUKUNFT: bewahren\n:END:\nText\n",
        )
        .unwrap();

        let mut edit = crate::core::heading_utils::open_edit(&file, &at_line(1), &crate::core::errors::subject::heading()).unwrap();
        edit.write(&crate::core::models::HeadingUpdatePayload {
            title: Some("Etwas anderes tun".into()),
            ..Default::default()
        })
        .unwrap();
        edit.commit().unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("Etwas anderes tun"), "{text}");
        assert!(text.contains(":AUS_DER_ZUKUNFT: bewahren"), "the unknown property was dropped:\n{text}");
    }

    /// Two deletions under two DIFFERENT ancestors -- the case the single
    /// -ancestor test above cannot reach.
    ///
    /// Each ancestor's LAST_ACTIVITY touch is spliced at the line number
    /// FLAT (the pre-delete parse) recorded for it. That holds for an
    /// ancestor of the deleted subtree itself, which always sits above it
    /// -- but not for a SECOND ancestor further down the file, which the
    /// first deletion has already moved up. Its block then lands on
    /// whatever occupies those lines now: the heading is written a second
    /// time and a real subtask is overwritten.
    #[test]
    fn apply_bulk_delete_shifts_a_later_ancestors_touch_by_what_was_removed_above_it() {
        let text = "* Projekt A                                                        :project:\n** TODO A1\n** TODO A2\n* Projekt B                                                        :project:\n** TODO B1\n** TODO B2\n";
        let flat = org_parser::parse_headings_from_lines(Path::new("projects.org"), text.lines());
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        apply_bulk_delete(&flat, &[at_line(line_of("A1")), at_line(line_of("B1"))], &mut lines).unwrap();

        let joined = lines.join("\n");
        assert_eq!(
            lines.iter().filter(|l| l.contains("Projekt B")).count(),
            1,
            "the second ancestor must be written once, not duplicated:\n{joined}"
        );
        assert!(joined.contains("A2"), "A2 was not selected and must survive:\n{joined}");
        assert!(joined.contains("B2"), "B2 was not selected and must survive:\n{joined}");
        assert!(!joined.contains("A1"), "A1 was selected and must be gone:\n{joined}");
        assert!(!joined.contains("B1"), "B1 was selected and must be gone:\n{joined}");
    }

    /// The write side (`apply_content_heading_offset`) only reads a
    /// `*`-run as a heading when whitespace or the line end follows it.
    /// adjust_heading_levels has to draw the same line: a refile that
    /// changes the level walks every line of the extracted subtree,
    /// content included, and `*Wichtig*` at column 0 is emphasis -- which
    /// is exactly how Emacs writes it into a file this app then refiles.
    #[test]
    fn adjust_heading_levels_leaves_inline_emphasis_alone() {
        let mut lines = vec![
            "* Aufgabe".to_string(),
            "*Wichtig* nicht vergessen".to_string(),
            "** Unteraufgabe".to_string(),
        ];

        adjust_heading_levels(&mut lines, 1);

        assert_eq!(lines[0], "** Aufgabe");
        assert_eq!(lines[1], "*Wichtig* nicht vergessen", "emphasis is not a heading");
        assert_eq!(lines[2], "*** Unteraufgabe");
    }

    /* --- a code block in a description is written exactly as typed --- */

    /// The reported corruption: org code quoted in a `#+BEGIN_SRC org`
    /// came back a few stars deeper after every save -- `***` became
    /// `*****`, then `*******`. The writer demoted the block's lines and
    /// the reader, rightly, never took anything off inside a block.
    ///
    /// Five saves in a row, each fed what the previous one read back: the
    /// block has to come out exactly as it went in.
    #[test]
    fn saving_a_description_leaves_a_code_block_untouched_however_often() {
        let quoted = "\
Beispiel:
#+BEGIN_SRC org
*** TODO zitiert :rust:
:PROPERTIES:
:URL: https://docs.rs/reqwest
:END:
#+BEGIN_SRC org
***** TODO noch tiefer
#+END_SRC
#+END_SRC";
        let mut heading = Heading { level: 2, state: Some("TODO".into()), title: "Bug".into(), ..Default::default() };
        heading.content = quoted.to_string();

        for round in 1..=5 {
            let payload = HeadingUpdatePayload { content: Some(heading.content.clone()), ..Default::default() };
            let written = build_org_heading_block(&heading, &payload);
            let reread = org_parser::parse_headings_from_lines(Path::new("t.org"), written.iter());
            assert_eq!(reread.len(), 1, "round {round}: the quote became tasks:\n{}", written.join("\n"));
            assert_eq!(reread[0].content, quoted, "round {round}: the block changed");
            heading = reread.into_iter().next().unwrap();
        }
    }

    /// A description `*`-line OUTSIDE a block still gets its demotion --
    /// the rule that keeps "* Notiz" from escaping as a sibling -- and
    /// comes back as typed. Only the inside of the block is exempt.
    #[test]
    fn a_star_line_outside_the_block_is_still_demoted_and_restored() {
        let typed = "* Notiz\n#+BEGIN_SRC org\n* zitiert\n#+END_SRC";
        let heading = Heading { level: 2, state: Some("TODO".into()), title: "T".into(), ..Default::default() };
        let payload = HeadingUpdatePayload { content: Some(typed.into()), ..Default::default() };
        let written = build_org_heading_block(&heading, &payload);

        assert!(written.contains(&"*** Notiz".to_string()), "demoted below the host: {written:?}");
        assert!(written.contains(&",* zitiert".to_string()), "the block's inside escaped the org way, not demoted: {written:?}");
        let reread = org_parser::parse_headings_from_lines(Path::new("t.org"), written.iter());
        assert_eq!(reread[0].content, typed);
    }

    /// Refiling shifts every heading of the moved subtree -- and must not
    /// shift the org code quoted inside it.
    #[test]
    fn refiling_does_not_shift_the_stars_inside_a_code_block() {
        let mut lines: Vec<String> = ["** TODO Aufgabe", "#+BEGIN_SRC org", "* zitiert", "#+END_SRC", "*** TODO Unteraufgabe"]
            .iter().map(|l| l.to_string()).collect();

        adjust_heading_levels(&mut lines, -1);

        assert_eq!(lines, vec!["* TODO Aufgabe", "#+BEGIN_SRC org", "* zitiert", "#+END_SRC", "** TODO Unteraufgabe"]);
    }

    /* --- a task a description turns into gets an id --- */

    fn written_and_reread(parent: &Heading, content: &str) -> Vec<Heading> {
        let payload = HeadingUpdatePayload { content: Some(content.into()), ..Default::default() };
        let written = build_org_heading_block(parent, &payload);
        org_parser::parse_headings_from_lines(Path::new("t.org"), written.iter())
    }

    /// The reported case: the same link pasted twice as a TODO. Both became
    /// real tasks with one title and no id -- and every write refuses a
    /// title that occurs twice without an id, so neither could be deleted,
    /// finished or moved. Each now gets its own id on the way in.
    #[test]
    fn two_identical_pasted_tasks_each_get_their_own_id() {
        let parent = Heading { level: 2, state: Some("TODO".into()), title: "konzept".into(), ..Default::default() };
        let pasted = "\
Notizen dazu.
* TODO [[https://docs.rs/reqwest][reqwest - Rust]] :rust:
:PROPERTIES:
:URL: https://docs.rs/reqwest
:END:
* TODO [[https://docs.rs/reqwest][reqwest - Rust]] :rust:
:PROPERTIES:
:URL: https://docs.rs/reqwest
:END:";
        let headings = written_and_reread(&parent, pasted);

        assert_eq!(headings.len(), 3, "parent and two tasks: {headings:?}");
        let ids: Vec<_> = headings[1..].iter().map(|h| heading_identity::custom_id_of(h).map(str::to_string)).collect();
        assert!(ids.iter().all(Option::is_some), "both have an id: {ids:?}");
        assert_ne!(ids[0], ids[1], "and not the same one");
        // The drawer that came with the paste is kept, not replaced.
        assert_eq!(headings[2].properties.get("URL").map(String::as_str), Some("https://docs.rs/reqwest"));

        // Which is what makes the second one addressable -- by id, the way
        // the delete would find it.
        let target = HeadingTarget { line_no: 0, custom_id: ids[1].clone(), title: Some(headings[2].title.clone()) };
        assert_eq!(heading_identity::resolve_target(&headings, &target, "t"), Ok(2));
    }

    /// A pasted task with an id of its own keeps it; one with none and no
    /// drawer gets a drawer -- after its planning line, where org wants it.
    #[test]
    fn an_existing_id_is_kept_and_a_missing_drawer_goes_after_the_planning_line() {
        let parent = Heading { level: 1, state: Some("TODO".into()), title: "P".into(), ..Default::default() };
        let pasted = "\
* TODO mit ID
:PROPERTIES:
:CUSTOM_ID: h:eigen
:END:
* TODO ohne Schublade
SCHEDULED: <2026-10-01 Thu>";
        let payload = HeadingUpdatePayload { content: Some(pasted.into()), ..Default::default() };
        let written = build_org_heading_block(&parent, &payload);
        let headings = org_parser::parse_headings_from_lines(Path::new("t.org"), written.iter());

        assert_eq!(heading_identity::custom_id_of(&headings[1]), Some("h:eigen"));
        assert!(heading_identity::custom_id_of(&headings[2]).is_some(), "{}", written.join("\n"));
        assert!(headings[2].scheduled.is_some(), "the planning line is still a planning line:\n{}", written.join("\n"));
    }

    /// A TODO line inside a code block is text -- no drawer is injected
    /// into org code someone is quoting.
    #[test]
    fn a_task_quoted_in_a_code_block_gets_nothing() {
        let parent = Heading { level: 2, state: Some("TODO".into()), title: "Bug".into(), ..Default::default() };
        let quoted = "#+BEGIN_SRC org\n*** TODO zitiert\n#+END_SRC";
        let headings = written_and_reread(&parent, quoted);

        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].content, quoted);
    }

    /* --- CLOSED: is stamped for every finished state, not just DONE --- */

    /// `WONT-DO` is a finished state (FINISHED_STATES), so it gets a
    /// CLOSED date like DONE does. Without one, find_archivable_ranges
    /// reads the missing date as "old enough" (see
    /// task_utils::is_older_than_n_days, which answers `true` for `None`)
    /// and archives an abandoned task on the very next run, whatever the
    /// configured delay says.
    #[test]
    fn finishing_a_task_as_wont_do_stamps_closed_like_done_does() {
        for state in ["DONE", "WONT-DO", "OBE"] {
            let heading = make_heading(1, 1, 1, "Aufgabe");
            let payload = HeadingUpdatePayload { state: Some(state.to_string()), ..Default::default() };

            let block = build_org_heading_block(&heading, &payload);

            assert!(
                block.iter().any(|l| l.contains("CLOSED: [")),
                "{state} is a finished state and must carry a CLOSED date: {block:?}"
            );
        }
    }

    /// Reopening clears it again -- for every finished state it can be
    /// reopened FROM, not only DONE.
    #[test]
    fn reopening_a_finished_task_drops_its_closed_date() {
        for state in ["DONE", "WONT-DO", "OBE"] {
            let mut heading = make_heading(1, 2, 1, "Aufgabe");
            heading.state = Some(state.to_string());
            heading.closed = Some("2026-08-01 Sat 09:00".to_string());
            let payload = HeadingUpdatePayload { state: Some("TODO".to_string()), ..Default::default() };

            let block = build_org_heading_block(&heading, &payload);

            assert!(
                !block.iter().any(|l| l.contains("CLOSED:")),
                "reopened from {state}, so the CLOSED date has to go: {block:?}"
            );
        }
    }

    /// Moving between two finished states keeps the date the task was
    /// actually finished on -- org's own org-log-done fires on entering
    /// the done set, not on moving around inside it. Re-dating here would
    /// also restart the archive delay for a task nobody touched the
    /// meaning of.
    #[test]
    fn moving_between_two_finished_states_keeps_the_original_closed_date() {
        let mut heading = make_heading(1, 2, 1, "Aufgabe");
        heading.state = Some("DONE".to_string());
        heading.closed = Some("2026-08-01 Sat 09:00".to_string());
        let payload = HeadingUpdatePayload { state: Some("WONT-DO".to_string()), ..Default::default() };

        let block = build_org_heading_block(&heading, &payload);

        assert!(
            block.iter().any(|l| l.contains("CLOSED: [2026-08-01 Sat 09:00]")),
            "the original finish date has to survive: {block:?}"
        );
    }

    /* --- FLAT and LINES that no longer agree --- */

    /// Both batch functions take their FLAT and their LINES from one read
    /// (`heading_utils::read_and_parse`), so a range that does not fit the
    /// lines means the two have drifted apart. That is refused rather than
    /// trimmed to fit -- a clamped range writes the block over whatever
    /// occupies those lines instead, or cuts the file off where the range
    /// started, and looks like a successful save.
    ///
    /// Nothing reaches disk either way: both callers propagate this with
    /// `?` before they get to `write_lines` (see
    /// commands_headings::bulk_update_headings).
    #[test]
    fn a_bulk_update_whose_range_no_longer_fits_the_lines_is_refused() {
        let text = "* TODO Eins\n* TODO Zwei\n* TODO Drei\n";
        let flat = org_parser::parse_headings_from_lines(Path::new("a.org"), text.lines());
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        // The file as it really is now -- shorter than the parse believes.
        let mut lines = vec!["* TODO Eins".to_string()];

        let updates = vec![BulkHeadingUpdate {
            target: at_line(line_of("Drei")),
            payload: HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() },
        }];
        let err = apply_bulk_update(&flat, &updates, &mut lines).unwrap_err();

        assert!(err.contains("Drei"), "the error names the heading it could not place: {err}");
        assert_eq!(lines, vec!["* TODO Eins".to_string()], "nothing was spliced");
    }

    #[test]
    fn a_bulk_delete_whose_range_no_longer_fits_the_lines_is_refused() {
        let text = "* TODO Eins\n* TODO Zwei\n* TODO Drei\n";
        let flat = org_parser::parse_headings_from_lines(Path::new("a.org"), text.lines());
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        let mut lines = vec!["* TODO Eins".to_string()];

        let err = apply_bulk_delete(&flat, &[at_line(line_of("Drei"))], &mut lines).unwrap_err();

        assert!(err.contains("Drei"), "the error names the heading it could not remove: {err}");
        assert_eq!(lines, vec!["* TODO Eins".to_string()], "nothing was drained");
    }

    /// The one read that makes the check above meaningful: the lines handed
    /// back are the ones the headings were parsed from, so their line
    /// numbers index into them by construction rather than by timing.
    #[test]
    fn read_and_parse_returns_lines_the_headings_actually_index_into() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.org");
        std::fs::write(&path, "* TODO Eins\nText\n* TODO Zwei\n").unwrap();

        let (lines, flat) = read_and_parse(&path).unwrap();

        assert_eq!(flat.len(), 2);
        for heading in &flat {
            assert!(heading.last_line <= lines.len(), "{} runs past the lines it was parsed from", heading.title);
            assert!(lines[heading.line_no - 1].contains(&heading.title), "line {} is not {}", heading.line_no, heading.title);
        }
    }
