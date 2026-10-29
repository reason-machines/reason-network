//! An opt-in local window over the existing daemon directory. No message consumption or agent registration.

#[cfg(all(windows, feature = "desktop"))]
mod windows;
#[cfg(all(windows, feature = "desktop"))]
pub use windows::run;

#[cfg(not(all(windows, feature = "desktop")))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("Desktop requires Windows and a build with --features desktop")
}

#[cfg(any(test, all(windows, feature = "desktop")))]
mod model {
    use crate::proto::{Directory, SessionInfo};
    #[cfg(all(windows, feature = "desktop"))]
    use crate::{client::Link, proto::Request, util};
    #[cfg(all(windows, feature = "desktop"))]
    use std::time::Duration;

    #[cfg(all(windows, feature = "desktop"))]
    pub enum Command {
        Refresh {},
        CopyAddress { address: String },
        CopyTeamLink {},
    }

    #[cfg(all(windows, feature = "desktop"))]
    pub struct Snapshot {
        pub directory: Option<Directory>,
        pub error: Option<&'static str>,
        pub updated_at: i64,
    }

    #[cfg(all(windows, feature = "desktop"))]
    pub async fn read(link: &std::sync::Arc<Link>) -> Snapshot {
        let directory =
            link.call_within(Request::Who { me: None, filter: None, all: false }, Duration::from_secs(2)).await.ok();
        Snapshot {
            error: directory
                .is_none()
                .then_some("Local daemon unavailable. Run reason-network who to start it, then refresh."),
            directory,
            updated_at: util::now_ms(),
        }
    }

    pub fn can_copy(directory: Option<&Directory>, address: &str) -> bool {
        !address.contains('\0') && directory.is_some_and(|d| d.sessions.iter().any(|s| s.address == address))
    }

    pub fn is_local(session: &SessionInfo, directory: &Directory) -> bool {
        session.person == directory.person && session.machine == directory.machine
    }

    pub fn visible_sessions<'a>(
        directory: &'a Directory,
        query: &str,
        working: bool,
        unread: bool,
    ) -> Vec<&'a SessionInfo> {
        let query = query.trim().to_lowercase();
        directory
            .sessions
            .iter()
            .filter(|s| {
                (!working || s.status == "busy")
                    && (!unread || (is_local(s, directory) && s.unread > 0))
                    && (query.is_empty()
                        || [
                            s.address.as_str(),
                            &s.name,
                            &s.person,
                            &s.machine,
                            &s.agent,
                            &s.cwd,
                            s.repo.as_deref().unwrap_or_default(),
                            s.branch.as_deref().unwrap_or_default(),
                            s.doing.as_deref().unwrap_or_default(),
                        ]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&query)))
            })
            .collect()
    }

    // Win32 controls terminate strings at NUL. Keep untrusted labels on one line and bound their display cost.
    // Clipboard addresses are validated and copied separately, never reconstructed from these labels.
    pub fn display_text(text: &str) -> String {
        text.chars()
            .take(512)
            .map(
                |c| {
                    if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                        ' '
                    } else {
                        c
                    }
                },
            )
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::proto::Tier;

        fn directory() -> Directory {
            let local = SessionInfo {
                address: "alice/项目-codex".into(),
                name: "项目".into(),
                person: "alice".into(),
                machine: "pc".into(),
                agent: "codex".into(),
                cwd: "C:\\项目 空格".into(),
                repo: Some("REASON".into()),
                branch: Some("feat/desktop".into()),
                status: "busy".into(),
                since: None,
                doing: Some("Review Unicode".into()),
                tier: Tier::Pull,
                unread: 3,
                is_me: false,
            };
            let mut remote = local.clone();
            remote.address = "alice/remote".into();
            remote.machine = "laptop".into();
            remote.status = "idle".into();
            let mut other = local.clone();
            other.address = "bob/codex".into();
            other.person = "bob".into();
            Directory {
                team: Some("test".into()),
                person: "alice".into(),
                machine: "pc".into(),
                machines: vec![],
                sessions: vec![local, remote, other],
            }
        }

        #[test]
        fn clipboard_addresses_must_match_a_complete_current_address() {
            let mut d = directory();
            assert!(can_copy(Some(&d), "alice/项目-codex"));
            for address in
                ["alice/项目", "Alice/项目-codex", "alice/项目-codex; powershell", "alice/项目-codex\0hidden"]
            {
                assert!(!can_copy(Some(&d), address));
            }
            assert!(!can_copy(None, "alice/项目-codex"));
            d.sessions[0].address.push('\0');
            assert!(!can_copy(Some(&d), &d.sessions[0].address));
        }

        #[test]
        fn unread_is_local_to_both_person_and_machine() {
            let d = directory();
            assert!(is_local(&d.sessions[0], &d));
            assert!(!is_local(&d.sessions[1], &d));
            assert!(!is_local(&d.sessions[2], &d));
            let rows = visible_sessions(&d, "", false, true);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].address, "alice/项目-codex");
        }

        #[test]
        fn filters_cover_unicode_paths_activity_and_case_insensitive_repo_names() {
            let d = directory();
            for query in ["项目 空格", "reason", "REVIEW unicode", "feat/desktop"] {
                assert_eq!(visible_sessions(&d, query, false, false).len(), 3);
            }
            assert_eq!(visible_sessions(&d, "", true, false).len(), 2);
            assert_eq!(visible_sessions(&d, "", true, true).len(), 1);
            assert!(visible_sessions(&d, "absent", false, false).is_empty());
            assert!(visible_sessions(&d, "remote", true, false).is_empty());
        }

        #[test]
        fn native_labels_keep_unicode_and_literal_markup_but_cannot_hide_after_nul() {
            assert_eq!(display_text("项目 🦀 <script> & ' \""), "项目 🦀 <script> & ' \"");
            assert_eq!(display_text("a\0b\nc\r\td\u{202e}e"), "a b c  d e");
            assert_eq!(display_text(&"🦀".repeat(600)).chars().count(), 512);
        }
    }
}
