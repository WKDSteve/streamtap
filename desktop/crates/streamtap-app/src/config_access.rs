//! Who besides the user can read `config.json` (SRV-14).
//!
//! The file holds the sink token. The app sets no ACL of its own: the
//! profile's rights on `%APPDATA%` (the user, SYSTEM, Administrators) are
//! enough (decision of 2026-10-31). At start the app reads the file's DACL
//! and warns when one of the broad groups *Everyone*, *Users* or
//! *Authenticated Users* may read it, e.g. after the folder was moved or its
//! rights were widened by hand.
//!
//! The decision ([`broad_readers`]) works on plain [`AceView`]s, so it is
//! tested with made-up DACLs; [`read_dacl`] fills them from the real file
//! through the `windows-permissions` crate.

use std::path::Path;

use serde::Serialize;

/// The broad groups by SID, with the stable id the UI translates, in the
/// order of the warning. `S-1-5-21-*-513` stands for the *Domain Users*
/// group of any domain (on a machine without a domain, its *None* group,
/// which holds every local account), see [`is_group`].
const BROAD_GROUPS: [(&str, &str); 8] = [
    ("S-1-1-0", "everyone"),
    ("S-1-5-32-545", "users"),
    ("S-1-5-11", "authenticated-users"),
    ("S-1-5-4", "interactive"),
    ("S-1-2-0", "local"),
    ("S-1-5-32-546", "guests"),
    ("S-1-5-21-*-513", "domain-users"),
    ("S-1-15-2-1", "all-app-packages"),
];
const EVERYONE: &str = "S-1-1-0";

/// Whether `sid` is the group `pattern` names: an exact SID, or with one `*`
/// standing for a domain's sub-authorities (one or more decimal parts).
fn is_group(sid: &str, pattern: &str) -> bool {
    let Some((prefix, rid)) = pattern.split_once("-*-") else {
        return sid == pattern;
    };
    sid.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(rid))
        .and_then(|rest| rest.strip_suffix('-'))
        .is_some_and(|domain| {
            domain
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
}

const FILE_READ_DATA: u32 = 0x0000_0001;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
/// Rights that read the file's content or let the holder grant itself that
/// (changing the DACL or taking ownership is as good as reading).
const REVEALING: [u32; 3] = [FILE_READ_DATA, WRITE_DAC, WRITE_OWNER];

/// Generic rights and the file rights they stand for (`FILE_GENERIC_*`,
/// `FILE_ALL_ACCESS`).
const GENERIC_MAP: [(u32, u32); 4] = [
    (0x8000_0000, 0x0012_0089),
    (0x4000_0000, 0x0012_0116),
    (0x2000_0000, 0x0012_00A0),
    (0x1000_0000, 0x001F_01FF),
];
/// The bits of a mask that are not generic rights.
const SPECIFIC: u32 = 0x0FFF_FFFF;

/// The file rights an allow entry's mask grants, generic rights mapped. A
/// generic right in an entry on the file itself is normally mapped when it is
/// written; counting it here errs on the side of a warning.
fn granted(mask: u32) -> u32 {
    GENERIC_MAP
        .iter()
        .filter(|(generic, _)| mask & generic != 0)
        .fold(mask & SPECIFIC, |m, (_, file)| m | file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AceKind {
    Allow,
    Deny,
    /// Audit, label and other entries that grant nothing.
    Other,
}

/// One entry of a DACL, as far as the decision needs it.
#[derive(Debug, Clone)]
pub(crate) struct AceView {
    pub(crate) kind: AceKind,
    /// Applies to children only, not to the file itself.
    pub(crate) inherit_only: bool,
    /// String form, `S-1-...`.
    pub(crate) sid: String,
    pub(crate) mask: u32,
}

/// What the start-up check found; the UI shows a warning for both variants.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum ConfigAccess {
    /// These broad groups can read the file (stable ids, see `BROAD_GROUPS`).
    Exposed { groups: Vec<&'static str> },
    /// The rights could not be read.
    Unknown { error: String },
}

impl ConfigAccess {
    /// The line for the app's log.
    pub(crate) fn log_line(&self, path: &Path) -> String {
        match self {
            Self::Exposed { groups } => format!(
                "warning: {} can be read by {}; it holds the sink token",
                path.display(),
                groups.join(", ")
            ),
            Self::Unknown { error } => format!(
                "warning: cannot check who can read {}: {error}",
                path.display()
            ),
        }
    }
}

/// The broad groups that can read a file with this DACL, in `BROAD_GROUPS`
/// order. `None` is no DACL at all: everyone has full access.
///
/// Entries are evaluated in order and bit by bit like Windows does: a group
/// can read when an allow for it grants one of the [`REVEALING`] rights
/// (reading the data, changing the DACL, taking ownership) that no earlier
/// deny for that group, or for *Everyone*, took away. Membership beyond that
/// is not resolved, so the answer errs on the side of a warning.
pub(crate) fn broad_readers(dacl: Option<&[AceView]>) -> Vec<&'static str> {
    let Some(aces) = dacl else {
        return BROAD_GROUPS.iter().map(|(_, id)| *id).collect();
    };
    BROAD_GROUPS
        .iter()
        .filter(|(sid, _)| can_read(aces, sid))
        .map(|(_, id)| *id)
        .collect()
}

fn can_read(aces: &[AceView], group: &str) -> bool {
    let mut denied = 0u32;
    for ace in aces.iter().filter(|a| !a.inherit_only) {
        match ace.kind {
            // Only the specific bits of a deny take something away (generic
            // ones are mapped when an entry is written to a file); this errs
            // on the side of a warning as well.
            AceKind::Deny if is_group(&ace.sid, group) || ace.sid == EVERYONE => {
                denied |= ace.mask & SPECIFIC;
            }
            AceKind::Allow if is_group(&ace.sid, group) => {
                let mask = granted(ace.mask);
                if REVEALING.iter().any(|r| mask & r != 0 && denied & r == 0) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Checks `path`: `None` when no broad group can read it.
pub(crate) fn check(path: &Path) -> Option<ConfigAccess> {
    // The crate panics on a few malformed descriptors (e.g. a present but
    // null DACL); that must not end the app.
    let read = std::panic::catch_unwind(|| read_dacl(path))
        .unwrap_or_else(|_| Err("the security descriptor could not be read".into()));
    match read {
        Ok(dacl) => {
            let groups = broad_readers(dacl.as_deref());
            (!groups.is_empty()).then_some(ConfigAccess::Exposed { groups })
        }
        Err(error) => Some(ConfigAccess::Unknown { error }),
    }
}

/// The DACL of `path`; `Ok(None)` when it has none.
#[cfg(windows)]
pub(crate) fn read_dacl(path: &Path) -> Result<Option<Vec<AceView>>, String> {
    use windows_permissions::constants::{AceFlags, AceType, SeObjectType, SecurityInformation};
    use windows_permissions::wrappers::{
        ConvertSecurityDescriptorToStringSecurityDescriptor, GetNamedSecurityInfo,
    };

    let sd = GetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .map_err(|e| e.to_string())?;
    // A NULL DACL (everyone has full access) makes the crate's `dacl()`
    // panic; SDDL names it before the first entry as `NO_ACCESS_CONTROL`.
    let sddl = ConvertSecurityDescriptorToStringSecurityDescriptor(&sd, SecurityInformation::Dacl)
        .map_err(|e| e.to_string())?;
    if is_null_dacl(&sddl.to_string_lossy()) {
        return Ok(None);
    }
    let Some(acl) = sd.dacl() else {
        return Ok(None);
    };
    let mut aces = Vec::new();
    for i in 0..acl.len() {
        let Some(ace) = acl.get_ace(i) else {
            break;
        };
        let kind = match ace.ace_type() {
            AceType::ACCESS_ALLOWED_ACE_TYPE
            | AceType::ACCESS_ALLOWED_CALLBACK_ACE_TYPE
            | AceType::ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
            | AceType::ACCESS_ALLOWED_OBJECT_ACE_TYPE => AceKind::Allow,
            AceType::ACCESS_DENIED_ACE_TYPE
            | AceType::ACCESS_DENIED_CALLBACK_ACE_TYPE
            | AceType::ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
            | AceType::ACCESS_DENIED_OBJECT_ACE_TYPE => AceKind::Deny,
            _ => AceKind::Other,
        };
        aces.push(AceView {
            kind,
            inherit_only: ace.flags().contains(AceFlags::InheritOnly),
            sid: ace.sid().map(ToString::to_string).unwrap_or_default(),
            mask: ace.mask().bits(),
        });
    }
    Ok(Some(aces))
}

/// Whether the DACL part of an SDDL string (`D:` flags, then the entries in
/// parentheses) is a NULL DACL. Only the flags are looked at, so the text of
/// a conditional entry cannot fake it.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_null_dacl(sddl: &str) -> bool {
    sddl.split('(')
        .next()
        .is_some_and(|flags| flags.contains("NO_ACCESS_CONTROL"))
}

/// Other platforms have no DACL; the app is built for Windows only.
#[cfg(not(windows))]
pub(crate) fn read_dacl(_path: &Path) -> Result<Option<Vec<AceView>>, String> {
    Ok(Some(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    const SYSTEM: &str = "S-1-5-18";
    const ADMINS: &str = "S-1-5-32-544";
    const FULL: u32 = 0x001F_01FF;
    const READ: u32 = 0x0012_00A9;
    const NONE: [&str; 0] = [];

    fn ace(kind: AceKind, sid: &str, mask: u32) -> AceView {
        AceView {
            kind,
            inherit_only: false,
            sid: sid.into(),
            mask,
        }
    }

    fn allow(sid: &str, mask: u32) -> AceView {
        ace(AceKind::Allow, sid, mask)
    }

    /// The profile's default: the user, SYSTEM and Administrators.
    fn profile() -> Vec<AceView> {
        vec![allow(SYSTEM, FULL), allow(ADMINS, FULL), allow(ME, FULL)]
    }

    /// A concrete SID for a `BROAD_GROUPS` entry (the pattern's `*` filled).
    fn example(pattern: &str) -> String {
        pattern.replace('*', "1111111111-2222222222-3333333333")
    }

    /// Every group id, in warning order.
    fn all_ids() -> Vec<&'static str> {
        BROAD_GROUPS.iter().map(|(_, id)| *id).collect()
    }

    #[test]
    fn the_wider_groups_and_any_domains_users_are_broad() {
        for (sid, id) in [
            ("S-1-5-4", "interactive"),
            ("S-1-2-0", "local"),
            ("S-1-5-32-546", "guests"),
            (
                "S-1-5-21-1111111111-2222222222-3333333333-513",
                "domain-users",
            ),
            ("S-1-5-21-7-513", "domain-users"),
            ("S-1-15-2-1", "all-app-packages"),
        ] {
            let mut dacl = profile();
            dacl.push(allow(sid, READ));
            assert_eq!(broad_readers(Some(&dacl)), [id], "{sid}");
        }
        // Not Domain Users: another RID of the domain, a cut or forged form.
        for sid in [
            ME,
            "S-1-5-21-513",
            "S-1-5-21--513",
            "S-1-5-21-1-x-513",
            "S-1-5-21-1-5130",
            "S-1-5-21-1-513-1",
            "S-1-5-32-513",
        ] {
            let mut dacl = profile();
            dacl.push(allow(sid, READ));
            assert_eq!(broad_readers(Some(&dacl)), NONE, "{sid}");
        }
    }

    #[test]
    fn the_profile_default_reaches_no_broad_group() {
        assert_eq!(broad_readers(Some(&profile())), NONE);
        assert_eq!(broad_readers(Some(&[])), NONE, "an empty DACL: nobody");
    }

    #[test]
    fn each_broad_group_with_read_rights_is_found() {
        for (pattern, id) in BROAD_GROUPS {
            let sid = example(pattern);
            for mask in [READ, FULL, 0x0000_0001, 0x8000_0000, 0x1000_0000] {
                let mut dacl = profile();
                dacl.push(allow(&sid, mask));
                assert_eq!(broad_readers(Some(&dacl)), [id], "{sid} {mask:#x}");
            }
        }
        let mut all = profile();
        all.extend(
            BROAD_GROUPS
                .iter()
                .rev()
                .map(|(pattern, _)| allow(&example(pattern), READ)),
        );
        assert_eq!(broad_readers(Some(&all)), all_ids());
    }

    #[test]
    fn no_dacl_means_everyone() {
        assert_eq!(broad_readers(None), all_ids());
    }

    #[test]
    fn rights_without_reading_the_content_do_not_count() {
        // READ_CONTROL | SYNCHRONIZE | FILE_READ_ATTRIBUTES; FILE_WRITE_DATA.
        for mask in [0x0012_0080, 0x0000_0002, 0] {
            let mut dacl = profile();
            dacl.push(allow("S-1-1-0", mask));
            assert_eq!(broad_readers(Some(&dacl)), NONE, "{mask:#x}");
        }
    }

    #[test]
    fn inherit_only_and_non_access_entries_do_not_count() {
        let mut dacl = profile();
        dacl.push(AceView {
            inherit_only: true,
            ..allow("S-1-5-32-545", READ)
        });
        dacl.push(ace(AceKind::Other, "S-1-1-0", FULL));
        assert_eq!(broad_readers(Some(&dacl)), NONE);
    }

    #[test]
    fn a_deny_before_the_allow_blocks_it_and_one_after_does_not() {
        let users = "S-1-5-32-545";
        let before = vec![ace(AceKind::Deny, users, READ), allow(users, READ)];
        assert_eq!(broad_readers(Some(&before)), NONE);
        let after = vec![allow(users, READ), ace(AceKind::Deny, users, READ)];
        assert_eq!(broad_readers(Some(&after)), ["users"]);
        // A deny of Everyone covers the other groups; one of another group not.
        let everyone = vec![
            ace(AceKind::Deny, "S-1-1-0", READ),
            allow(users, READ),
            allow("S-1-5-11", READ),
        ];
        assert_eq!(broad_readers(Some(&everyone)), NONE);
        let other = vec![ace(AceKind::Deny, ME, READ), allow(users, READ)];
        assert_eq!(broad_readers(Some(&other)), ["users"]);
        // A deny of writing only blocks no reading, nor does a generic one.
        let write_only = vec![ace(AceKind::Deny, users, 0x0000_0002), allow(users, READ)];
        assert_eq!(broad_readers(Some(&write_only)), ["users"]);
        let generic = vec![ace(AceKind::Deny, users, 0x8000_0000), allow(users, READ)];
        assert_eq!(broad_readers(Some(&generic)), ["users"]);
    }

    /// Changing the DACL or taking ownership lets a group grant itself
    /// reading, so it counts like reading; a deny takes away only its bits.
    #[test]
    fn rights_to_change_the_dacl_or_owner_count_and_denies_work_bit_by_bit() {
        let users = "S-1-5-32-545";
        for mask in [WRITE_DAC, WRITE_OWNER] {
            let mut dacl = profile();
            dacl.push(allow(users, mask));
            assert_eq!(broad_readers(Some(&dacl)), ["users"], "{mask:#x}");
        }
        // A deny of changing the DACL does not block reading.
        let dac = vec![ace(AceKind::Deny, users, WRITE_DAC), allow(users, READ)];
        assert_eq!(broad_readers(Some(&dac)), ["users"]);
        // Reading denied, but full control still includes changing the DACL.
        let read_only = vec![ace(AceKind::Deny, users, READ), allow(users, FULL)];
        assert_eq!(broad_readers(Some(&read_only)), ["users"]);
        // All three denied (by Everyone): nothing left.
        let all = vec![
            ace(
                AceKind::Deny,
                "S-1-1-0",
                FILE_READ_DATA | WRITE_DAC | WRITE_OWNER,
            ),
            allow(users, FULL),
            allow(users, 0x1000_0000),
        ];
        assert_eq!(broad_readers(Some(&all)), NONE);
    }

    #[test]
    fn a_null_dacl_is_recognised_by_its_flags_only() {
        for sddl in ["D:NO_ACCESS_CONTROL", "D:PNO_ACCESS_CONTROL"] {
            assert!(is_null_dacl(sddl), "{sddl}");
        }
        for sddl in [
            "D:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
            "D:",
            "",
            "D:(XA;;FR;;;WD;(@User.NO_ACCESS_CONTROL == 1))",
        ] {
            assert!(!is_null_dacl(sddl), "{sddl}");
        }
    }

    /// A real file with a NULL DACL (full access for everyone) is reported
    /// as readable by every broad group, not as unchecked.
    #[cfg(windows)]
    #[test]
    fn a_real_file_with_a_null_dacl_is_readable_by_everyone() {
        use windows_permissions::constants::{SeObjectType, SecurityInformation};
        use windows_permissions::wrappers::SetNamedSecurityInfo;

        let dir = std::env::temp_dir().join(format!("streamtap-nulldacl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        std::fs::write(&file, b"{}").unwrap();
        SetNamedSecurityInfo(
            file.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(read_dacl(&file).unwrap().is_none(), "a NULL DACL");
        assert_eq!(
            check(&file),
            Some(ConfigAccess::Exposed { groups: all_ids() })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_lines_name_the_file_and_the_groups() {
        let path = Path::new("C:/x/config.json");
        let exposed = ConfigAccess::Exposed {
            groups: vec!["everyone", "users"],
        };
        let line = exposed.log_line(path);
        assert!(
            line.contains("config.json") && line.contains("everyone, users"),
            "{line}"
        );
        let unknown = ConfigAccess::Unknown {
            error: "denied".into(),
        };
        assert!(unknown.log_line(path).ends_with("denied"));
        let json = serde_json::to_value(&exposed).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"state": "exposed", "groups": ["everyone", "users"]})
        );
    }

    /// The real file system: a fresh file in a fresh folder is checked, then
    /// Everyone is granted read with `icacls` (by SID, independent of the
    /// Windows language) and the check finds it.
    #[cfg(windows)]
    #[test]
    fn the_real_dacl_of_a_file_is_read_and_a_grant_to_everyone_is_found() {
        let dir = std::env::temp_dir().join(format!("streamtap-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        std::fs::write(&file, b"{}").unwrap();
        let dacl = read_dacl(&file).unwrap().expect("a file has a DACL");
        assert!(!dacl.is_empty());
        assert!(dacl.iter().all(|a| a.sid.starts_with("S-1-")), "{dacl:?}");
        let before = check(&file);
        assert!(
            !matches!(&before, Some(ConfigAccess::Exposed { groups }) if groups.contains(&"everyone")),
            "{before:?}"
        );
        let status = std::process::Command::new("icacls")
            .arg(&file)
            .args(["/grant", "*S-1-1-0:(R)"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        match check(&file) {
            Some(ConfigAccess::Exposed { groups }) => assert!(groups.contains(&"everyone")),
            other => panic!("Everyone may read the file now: {other:?}"),
        }
        let missing = check(&dir.join("missing.json"));
        assert!(
            matches!(missing, Some(ConfigAccess::Unknown { .. })),
            "{missing:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
