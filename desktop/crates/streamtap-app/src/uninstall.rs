//! `streamtap-app --uninstall`: the app removes the install it is part of, in its own process.
//!
//! This is what the entry in Settings > Apps runs: no PowerShell is started and no console window
//! shows. The work is `appkit-uninstall`'s (the question in a window, the checks, the shortcuts,
//! the files, the entry last); here is what only streamtap has: its programs in `bin\`, the
//! extension folder, and the keys of the browsers that lead to the host of this install.
//! `scripts/uninstall.ps1` stays for the terminal, and for `-RemoveData`: the app never removes
//! settings, pairings or captures.

use std::path::Path;

use appkit_uninstall::{Product, Step};

use crate::registration::Browser;

/// streamtap as `scripts/install.ps1` lays it out (and the programs of the layout before `bin\`).
pub(crate) const PRODUCT: Product = Product {
    name: "streamtap",
    program: "streamtap-app.exe",
    program_dir: "bin",
    link: "streamtap.lnk",
    programs: &[
        "streamtap-app.exe",
        "streamtap-sink.exe",
        "streamtap-host.exe",
    ],
    files: &[
        r"bin\streamtap-app.exe",
        r"bin\streamtap-sink.exe",
        r"bin\streamtap-host.exe",
        "streamtap-app.exe",
        "streamtap-sink.exe",
        "streamtap-host.exe",
        "uninstall.ps1",
        "LICENSE",
        "README.md",
        "VERSION",
    ],
    folders: &["extension"],
    run_value: None,
    env: "STREAMTAP",
    close: "close its window; it stops its sink",
    kept: "your settings, pairings and captures",
    first: Some(forget_browsers),
};

/// The host a native messaging manifest names; `None` when the file cannot be read.
fn manifest_host(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let value: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    value["path"].as_str().map(str::to_owned)
}

/// Removes the key of each browser whose manifest names a host inside this install, with the
/// manifest; the registration of another copy (a checkout) stays, and is named.
fn forget_browsers(step: &mut Step<'_>) -> Result<(), String> {
    for browser in Browser::ALL {
        let key = browser.key_below_software();
        let Some(manifest) = step.registry_text(&key, "") else {
            continue;
        };
        let name = browser.title();
        let manifest = Path::new(&manifest);
        match manifest_host(manifest) {
            Some(host) if step.inside(Path::new(&host)) => {
                step.remove_registry_key(&key)?;
                if manifest.is_file() {
                    step.remove_file(manifest)?;
                }
                step.removed(format!("the {name} registration"));
            }
            Some(host) => step.kept(format!(
                "kept the {name} registration (it starts {host}, not this install)"
            )),
            None => step.kept(format!(
                "kept the {name} registration (its manifest {} cannot be read)",
                manifest.display()
            )),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_product_fits_together() {
        assert_eq!(PRODUCT.check(), Ok(()));
    }

    #[test]
    fn the_host_of_a_manifest_is_read_as_the_app_writes_it() {
        let dir = std::env::temp_dir().join(format!(
            "streamtap-uninstall-manifest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("chrome.json");
        let host = Path::new(r"C:\Apps\streamtap\bin\streamtap-host.exe");
        std::fs::write(&file, crate::registration::manifest_json(host).unwrap()).unwrap();
        assert_eq!(manifest_host(&file).as_deref(), host.to_str());
        // With the mark some editors put before the text.
        std::fs::write(&file, format!("\u{feff}{}", r#"{"path": "C:\\x.exe"}"#)).unwrap();
        assert_eq!(manifest_host(&file).as_deref(), Some(r"C:\x.exe"));
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(manifest_host(&file), None);
        assert_eq!(manifest_host(&dir.join("missing.json")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
