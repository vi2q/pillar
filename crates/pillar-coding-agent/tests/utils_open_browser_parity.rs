//! Parity test for utils/open-browser.ts (pi v0.84.3): the platform →
//! launcher decision. The spawn itself is best-effort and untested by
//! upstream too; what matters is that the target stays a single argv entry
//! (never shell-parsed) and that each platform gets its documented launcher.

use pillar_coding_agent::utils::open_browser::{OpenBrowserPlatform, launcher_command};

#[test]
fn the_platform_launcher_is_the_documented_one() {
    // Metacharacters stay inside one argv entry: no shell ever sees them.
    let target = "https://example.test/cb?code=a&state=b|c";

    assert_eq!(
        launcher_command(OpenBrowserPlatform::Darwin, target),
        ("open".to_string(), vec![target.to_string()])
    );
    assert_eq!(
        launcher_command(OpenBrowserPlatform::Win32, target),
        (
            "rundll32".to_string(),
            vec![
                "url.dll,FileProtocolHandler".to_string(),
                target.to_string()
            ]
        )
    );
    assert_eq!(
        launcher_command(OpenBrowserPlatform::Other, target),
        ("xdg-open".to_string(), vec![target.to_string()])
    );
}

#[test]
fn the_native_platform_matches_the_build_target() {
    let expected = if cfg!(target_os = "macos") {
        OpenBrowserPlatform::Darwin
    } else if cfg!(target_os = "windows") {
        OpenBrowserPlatform::Win32
    } else {
        OpenBrowserPlatform::Other
    };
    assert_eq!(OpenBrowserPlatform::native(), expected);
}
