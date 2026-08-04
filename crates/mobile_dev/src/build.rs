//! Build command shaping for the mobile_dev panel.
//!
//! [`build_command`] maps a [`BuildKind`] + [`MobilePlatform`] to the program
//! and arguments to run (`npx expo run:*`, `eas build ...`); the panel runs the
//! result in an interactive terminal tab.

/// Which mobile OS a build targets. iOS builds only work on macOS hosts;
/// callers gate on that before offering them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MobilePlatform {
    Android,
    Ios,
}

impl MobilePlatform {
    pub fn label(self) -> &'static str {
        match self {
            Self::Android => "Android",
            Self::Ios => "iOS",
        }
    }

    fn eas_flag(self) -> &'static str {
        match self {
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }
}

/// What kind of build the user asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildKind {
    /// `npx expo run:<platform> --device [<id>]`. Builds a debug app,
    /// installs to the device, and launches. Requires Metro running at
    /// runtime; the bundled JS expects to reach the dev server on launch.
    LocalDebugRun,
    /// `eas build --platform <platform> --profile preview --non-interactive`.
    /// Cloud build that yields a standalone install link.
    EasPreview,
    /// `eas build --platform <platform> --profile production --non-interactive`.
    EasProduction,
}

impl BuildKind {
    pub fn label(self, platform: MobilePlatform) -> String {
        match self {
            Self::LocalDebugRun => format!("Run on {} device (debug)", platform.label()),
            Self::EasPreview => format!("EAS preview build ({})", platform.label()),
            Self::EasProduction => format!("EAS production build ({})", platform.label()),
        }
    }
}

pub fn build_command(
    kind: BuildKind,
    platform: MobilePlatform,
    device_id: Option<&str>,
) -> (String, Vec<String>) {
    match kind {
        BuildKind::LocalDebugRun => {
            let subcommand = match platform {
                MobilePlatform::Android => "run:android",
                MobilePlatform::Ios => "run:ios",
            };
            let mut args = vec!["expo".to_string(), subcommand.to_string()];
            args.push("--device".to_string());
            if let Some(id) = device_id {
                args.push(id.to_string());
            }
            ("npx".to_string(), args)
        }
        BuildKind::EasPreview | BuildKind::EasProduction => {
            let profile = match kind {
                BuildKind::EasPreview => "preview",
                _ => "production",
            };
            (
                "eas".to_string(),
                vec![
                    "build".to_string(),
                    "--platform".to_string(),
                    platform.eas_flag().to_string(),
                    "--profile".to_string(),
                    profile.to_string(),
                    "--non-interactive".to_string(),
                ],
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_debug_command_has_device_flag() {
        let (program, args) =
            build_command(BuildKind::LocalDebugRun, MobilePlatform::Android, Some("ABC123"));
        assert_eq!(program, "npx");
        assert!(args.iter().any(|a| a == "run:android"));
        assert!(args.iter().any(|a| a == "ABC123"));
    }

    #[test]
    fn local_debug_command_without_serial_falls_back_to_interactive_picker() {
        let (program, args) = build_command(BuildKind::LocalDebugRun, MobilePlatform::Android, None);
        assert_eq!(program, "npx");
        // `expo run:android --device` (no serial) prompts the user to pick one.
        let device_idx = args.iter().position(|a| a == "--device").unwrap();
        assert_eq!(
            args.get(device_idx + 1),
            None,
            "trailing --device should let expo prompt"
        );
    }

    #[test]
    fn local_debug_ios_command_targets_udid() {
        let (program, args) = build_command(
            BuildKind::LocalDebugRun,
            MobilePlatform::Ios,
            Some("ABCD-1234"),
        );
        assert_eq!(program, "npx");
        assert!(args.iter().any(|a| a == "run:ios"));
        let device_idx = args.iter().position(|a| a == "--device").unwrap();
        assert_eq!(args.get(device_idx + 1).map(String::as_str), Some("ABCD-1234"));
    }

    #[test]
    fn eas_preview_command_shape() {
        let (program, args) = build_command(BuildKind::EasPreview, MobilePlatform::Android, None);
        assert_eq!(program, "eas");
        assert_eq!(args[0], "build");
        assert!(args.iter().any(|a| a == "android"));
        assert!(args.iter().any(|a| a == "preview"));
        assert!(args.iter().any(|a| a == "--non-interactive"));
    }

    #[test]
    fn eas_production_command_shape() {
        let (program, args) = build_command(BuildKind::EasProduction, MobilePlatform::Android, None);
        assert_eq!(program, "eas");
        assert!(args.iter().any(|a| a == "production"));
    }

    #[test]
    fn eas_ios_command_targets_ios_platform() {
        let (program, args) = build_command(BuildKind::EasPreview, MobilePlatform::Ios, None);
        assert_eq!(program, "eas");
        let platform_idx = args.iter().position(|a| a == "--platform").unwrap();
        assert_eq!(args.get(platform_idx + 1).map(String::as_str), Some("ios"));
    }


    #[test]
    fn build_kind_labels() {
        assert!(
            BuildKind::LocalDebugRun
                .label(MobilePlatform::Android)
                .contains("debug")
        );
        assert!(
            BuildKind::EasPreview
                .label(MobilePlatform::Android)
                .contains("preview")
        );
        assert!(
            BuildKind::EasProduction
                .label(MobilePlatform::Ios)
                .contains("iOS")
        );
    }

}
