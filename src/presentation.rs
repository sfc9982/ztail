//! CLI presentation only: never interpret or recolor log payloads.
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;

use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Language {
    #[default]
    #[value(name = "en")]
    English,
    #[value(name = "zh-CN", alias = "zh-cn", alias = "zh")]
    Chinese,
}

impl Language {
    pub fn detect() -> Self {
        // POSIX locale precedence; an empty variable does not override the next.
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Some(raw) = std::env::var_os(key).filter(|v| !v.is_empty()) {
                return Self::from_locale(&raw.to_string_lossy());
            }
        }
        Self::English
    }

    fn from_locale(raw: &str) -> Self {
        let base = raw.split(['.', '@']).next().unwrap_or("");
        if [
            "zh", "zh_CN", "zh-CN", "zh_SG", "zh-SG", "zh-Hans", "zh_Hans",
        ]
        .iter()
        .any(|candidate| base.eq_ignore_ascii_case(candidate))
        {
            Self::Chinese
        } else {
            Self::English
        }
    }

    pub fn text<'a>(self, en: &'a str, zh: &'a str) -> &'a str {
        match self {
            Self::English => en,
            Self::Chinese => zh,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

impl Color {
    pub fn enabled(self, terminal: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => {
                let nonempty = |key| std::env::var_os(key).is_some_and(|v| !v.is_empty());
                if nonempty("NO_COLOR") {
                    return false;
                }
                if nonempty("CLICOLOR_FORCE")
                    && std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0")
                {
                    return true;
                }
                terminal
                    && std::env::var_os("TERM").is_none_or(|v| v != "dumb")
                    && std::env::var_os("CLICOLOR").is_none_or(|v| v != "0")
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Presentation {
    pub language: Language,
    pub stdout_color: bool,
    pub stderr_color: bool,
}

static SETTINGS: OnceLock<Presentation> = OnceLock::new();

pub fn initialize(language: Language, color: Color) -> Presentation {
    let settings = Presentation {
        language,
        stdout_color: color.enabled(io::stdout().is_terminal()),
        stderr_color: color.enabled(io::stderr().is_terminal()),
    };
    let _ = SETTINGS.set(settings);
    settings
}

pub fn settings() -> Presentation {
    SETTINGS.get().copied().unwrap_or_default()
}

pub fn text(en: &str, zh: &str) -> String {
    settings().language.text(en, zh).into()
}

pub fn warn(message: &str) {
    let mut stderr = io::stderr().lock();
    // Best effort diagnostics must not panic when stderr has closed.
    if settings().stderr_color {
        let _ = writeln!(stderr, "\x1b[33m{message}\x1b[0m");
    } else {
        let _ = writeln!(stderr, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locale_matching_is_explicit() {
        for locale in ["zh_CN.UTF-8", "zh-CN", "zh_SG.utf8", "zh_Hans"] {
            assert_eq!(Language::from_locale(locale).text("en", "中"), "中");
        }
        for locale in [
            "C",
            "C.UTF-8",
            "en_US.UTF-8",
            "zh_TW.UTF-8",
            "zh-not-a-locale",
        ] {
            assert_eq!(Language::from_locale(locale).text("en", "中"), "en");
        }
    }
}
