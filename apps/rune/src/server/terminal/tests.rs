#[cfg(unix)]
mod unix {
    use std::fs;
    use std::time::Duration;

    use super::super::*;

    fn open_input(thread_id: &str) -> OpenTerminal {
        OpenTerminal {
            key: TerminalKey {
                thread_id: thread_id.to_owned(),
                terminal_id: "term-1".to_owned(),
            },
            cwd: std::env::current_dir().unwrap(),
            worktree_path: None,
            cols: None,
            rows: None,
            env: BTreeMap::from([("T3CODE_TERMINAL_SHELL".to_owned(), "/bin/sh".to_owned())]),
        }
    }

    async fn before_exit(events: &mut broadcast::Receiver<TerminalEvent>) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = String::new();
            loop {
                let event = events.recv().await.unwrap();
                match event.event {
                    OutputEvent::Output { data } => output.push_str(&data),
                    OutputEvent::Exited { .. } => {
                        return (output, serde_json::to_value(event).unwrap());
                    }
                    OutputEvent::Error { message } => panic!("Terminal reported error: {message}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("the interactive terminal exited")
    }

    #[tokio::test]
    async fn opens_a_native_pty_and_streams_the_resized_grid_with_exit_status() {
        let terminals = TerminalManager::default();
        let input = open_input("thread-one");
        let key = input.key.clone();
        let opened = terminals.open(input).unwrap();
        let opened = serde_json::to_value(opened).unwrap();
        assert_eq!(opened["status"], "running");
        assert_eq!(opened["cols"], 120);
        assert_eq!(opened["rows"], 30);
        assert!(opened["pid"].as_u64().is_some_and(|pid| pid > 0));

        let (snapshot, mut events) = terminals.observe(&key).unwrap();
        terminals.resize(&key, 91, 31).unwrap();
        terminals.write(&key, "printf 'grid:%s\\n' \"$(stty size)\"; printf 'ready:%s\\n' \"$T3CODE_THREAD_ID\"; exit 7\n", false).unwrap();
        let (output, exit) = before_exit(&mut events).await;
        assert!(output.contains("grid:31 91"), "{output:?}");
        assert!(output.contains("ready:thread-one"), "{output:?}");
        assert_eq!(exit["threadId"], "thread-one");
        assert_eq!(exit["terminalId"], "term-1");
        assert_eq!(exit["exitCode"], 7);
        assert!(exit["sequence"].as_u64().unwrap() > snapshot.sequence);

        let (exited, _) = terminals.observe(&key).unwrap();
        assert!(exited.history.contains("grid:31 91"));
        assert_eq!((exited.cols, exited.rows), (91, 31));
        terminals.clear(&key).unwrap();
        let (cleared, _) = terminals.observe(&key).unwrap();
        assert!(cleared.history.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn observed_open_keeps_a_background_job_alive_and_closes_when_idle() {
        let terminals = TerminalManager::default();
        let input = open_input("observed-shell");
        let key = input.key.clone();
        let (snapshot, mut events) = terminals.open_observed(input, true).unwrap();
        assert_eq!(snapshot.shell(), Some(Path::new("/bin/sh")));
        assert!(
            serde_json::to_value(snapshot)
                .unwrap()
                .get("shell")
                .is_none()
        );
        terminals
            .write(
                &key,
                "sleep 30 & job=$!; printf 'WORKTREE_%s\\n' BUSY\n",
                false,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = String::new();
            while !output.contains("WORKTREE_BUSY") {
                let event = events.recv().await.unwrap();
                if let OutputEvent::Output { data } = event.output() {
                    output.push_str(data);
                }
            }
        })
        .await
        .expect("the test shell started a live job");
        assert_eq!(terminals.has_active_subprocess(&key).unwrap(), Some(true));
        assert!(!terminals.close_idle(&key).unwrap());

        terminals
            .write(&key, "kill \"$job\"; wait \"$job\" 2>/dev/null\n", false)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while terminals.has_active_subprocess(&key).unwrap() != Some(false) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the test shell returned to its prompt");
        assert!(terminals.close_idle(&key).unwrap());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(events.recv().await.unwrap().output(), OutputEvent::Closed) {
                    return;
                }
            }
        })
        .await
        .expect("closing an idle shell publishes its close event");
    }

    #[tokio::test]
    async fn same_terminal_id_in_another_thread_survives_close() {
        let terminals = TerminalManager::default();
        let first = open_input("thread-first");
        let first_key = first.key.clone();
        let second = open_input("thread-second");
        let second_key = second.key.clone();
        let first_pid = terminals.open(first).unwrap().pid;
        let second_pid = terminals.open(second).unwrap().pid;
        assert_ne!(first_pid, second_pid);

        let (_, mut events) = terminals.observe(&second_key).unwrap();
        terminals.close(&first_key.thread_id, Some(&first_key.terminal_id), false);
        assert!(matches!(
            terminals.observe(&first_key),
            Err(TerminalError::Unknown(_))
        ));
        terminals
            .write(
                &second_key,
                "printf 'owner:%s\\n' \"$T3CODE_THREAD_ID\"; exit\n",
                true,
            )
            .unwrap();
        let (output, _) = before_exit(&mut events).await;
        assert!(output.contains("owner:thread-second"), "{output:?}");
    }

    struct StateDirectory(PathBuf);

    impl StateDirectory {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("t3-terminal-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn logs(&self) -> PathBuf {
            self.0.join("logs/terminals")
        }
    }

    impl Drop for StateDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn restores_pty_output_across_manager_restart_and_deletes_only_requested_history() {
        let directory = StateDirectory::new();
        let terminal = TerminalManager::with_state_dir(&directory.0).unwrap();
        let input = open_input("pty:restart");
        let key = input.key.clone();
        let path = directory.logs().join("terminal_cHR5OnJlc3RhcnQ.log");
        terminal.open(input).unwrap();
        let (_, mut events) = terminal.observe(&key).unwrap();
        terminal
            .write(
                &key,
                "printf 'persisted-%s\\n' actual; printf '\\033[6n'\n",
                false,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(data) = fs::read_to_string(&path)
                    && data.contains("persisted-actual")
                {
                    assert!(!data.contains("\x1b[6n"));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("history was stored while the terminal remained running");
        assert!(terminal.observe(&key).unwrap().0.is_running());
        terminal.write(&key, "exit\n", false).unwrap();
        let (raw, _) = before_exit(&mut events).await;
        assert!(raw.contains("persisted-actual"));
        assert!(raw.contains("\x1b[6n"));
        let (exited, _) = terminal.observe(&key).unwrap();
        assert!(!exited.history.contains("\x1b[6n"));
        drop(terminal);

        let persisted = fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("persisted-actual"));
        assert!(!persisted.contains("\x1b[6n"));
        let other = directory.logs().join("unrelated.log");
        fs::write(&other, "separate history\n").unwrap();

        let reopened = TerminalManager::with_state_dir(&directory.0).unwrap();
        let snapshot = reopened.open(open_input("pty:restart")).unwrap();
        assert_eq!(snapshot.sequence, 1);
        assert!(snapshot.history.contains("persisted-actual"));
        assert!(!snapshot.history.contains("\x1b[6n"));
        reopened.clear(&key).unwrap();
        let (cleared, _) = reopened.observe(&key).unwrap();
        assert!(!cleared.history.contains("persisted-actual"));
        reopened.close(&key.thread_id, Some(&key.terminal_id), true);
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(other).unwrap(), "separate history\n");
    }

    #[test]
    fn migrates_a_utf16_legacy_filename_and_resets_inherited_terminal_modes() {
        let directory = StateDirectory::new();
        let manager = TerminalManager::with_state_dir(&directory.0).unwrap();
        let legacy = directory.logs().join("legacy_______name.log");
        let history = "before\n\x1b[?47hlegacy screen\n";
        fs::write(&legacy, history).unwrap();
        let snapshot = manager.open(open_input("legacy:/😀///name")).unwrap();
        assert!(snapshot.history.starts_with(history));
        assert!(snapshot.history.contains("\x1b7\x1b[?47l"));
        assert!(snapshot.history.ends_with("\x1b[>4m"));
        assert!(!legacy.exists());
        assert_eq!(fs::read_dir(directory.logs()).unwrap().count(), 1);
    }

    #[test]
    fn refuses_to_read_a_terminal_log_symlink() {
        use std::os::unix::fs::symlink;

        let directory = StateDirectory::new();
        let manager = TerminalManager::with_state_dir(&directory.0).unwrap();
        let outside = directory.0.join("separate-file");
        fs::write(&outside, "unrelated data").unwrap();
        symlink(
            &outside,
            directory.logs().join("terminal_cHR5OnJlc3RhcnQ.log"),
        )
        .unwrap();
        assert!(matches!(
            manager.open(open_input("pty:restart")),
            Err(TerminalError::History(_, "read", _))
        ));
        assert_eq!(fs::read_to_string(outside).unwrap(), "unrelated data");
    }

    #[test]
    fn disk_tail_cuts_preserve_only_complete_terminal_and_utf8_sequences() {
        let directory = StateDirectory::new();
        let file = directory.0.join("legacy.log");
        for (original, restored) in [
            ("\x1b]0;x\x07ABCD", "ABCD"),
            ("名\x1b]0;名\x07ABCD", "ABCD"),
            ("\x1b]0;x\x07\x1b[31mABCD", "ABCD"),
            ("A😀🚀!", "🚀!"),
        ] {
            fs::write(&file, original).unwrap();
            assert_eq!(
                persistence::read_tail(&file, 8).unwrap(),
                (restored.to_owned(), true)
            );
        }
    }

    #[test]
    fn line_history_and_persistence_cursor_bound_append_and_force_compaction() {
        let mut state = History::with_limits(3, 32);
        state.append("drop\n\nline3");
        let initial = state.persistence_update(None);
        assert!(!initial.append);
        state.append("-continued\nline4");
        let appended = state.persistence_update(Some(initial.cursor));
        assert!(appended.append);
        assert_eq!(appended.data, "-continued\nline4");
        assert_eq!(state.text(), "\nline3-continued\nline4");
        assert_eq!(state.persistence_update(None).data, state.text());
        state.clear();
        state.append("new 🐱\n");
        let reset = state.persistence_update(Some(appended.cursor));
        assert!(!reset.append);
        assert_eq!(reset.data, "new 🐱\n");
    }
}
