//! Translation setup and helpers. Strings are marked with `gettext` (or
//! `ngettext` for counts); `po/update-pot.sh` extracts them into
//! `po/phodder.pot`. Placeholders are named, `{name}`, so a translation can
//! reorder them.

pub use gettextrs::{gettext, ngettext};

/// The gettext domain: `<locale dir>/<lang>/LC_MESSAGES/phodder.mo`.
pub const GETTEXT_PACKAGE: &str = "phodder";

/// Where compiled translations are installed. The Flatpak installs them
/// under /app; PHODDER_LOCALEDIR at build time overrides it.
const LOCALE_DIR: &str = match option_env!("PHODDER_LOCALEDIR") {
    Some(dir) => dir,
    None => "/app/share/locale",
};

/// The first thing main() does: setlocale isn't thread-safe, and logging
/// starts a thread. Failures go to stderr, since logging isn't set up yet.
pub fn init() {
    use gettextrs::{bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory};
    // SAFETY: called from main() before any other thread exists.
    unsafe {
        setlocale(LocaleCategory::LcAll, "");
    }
    if let Err(e) = bindtextdomain(GETTEXT_PACKAGE, LOCALE_DIR) {
        eprintln!("couldn't bind the translation domain to {LOCALE_DIR}: {e}");
    }
    if let Err(e) = bind_textdomain_codeset(GETTEXT_PACKAGE, "UTF-8") {
        eprintln!("couldn't set the translation codeset: {e}");
    }
    if let Err(e) = textdomain(GETTEXT_PACKAGE) {
        eprintln!("couldn't select the translation domain: {e}");
    }
}

/// Marks a string for extraction without translating it, for tables built
/// before translation is set up. Translate it with `gettext` where it's
/// shown.
#[allow(non_snake_case)]
pub const fn N_(msgid: &str) -> &str {
    msgid
}

/// `gettext`, then fills `{name}` placeholders.
pub fn gettext_f(msgid: &str, args: &[(&str, &str)]) -> String {
    fill(gettext(msgid), args)
}

/// `ngettext`, then fills `{name}` placeholders. `{n}` isn't filled
/// automatically; pass it in `args` when the string shows the count.
pub fn ngettext_f(singular: &str, plural: &str, n: u32, args: &[(&str, &str)]) -> String {
    fill(ngettext(singular, plural, n), args)
}

fn fill(mut text: String, args: &[(&str, &str)]) -> String {
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_fill_by_name_in_any_order() {
        assert_eq!(fill("{b} then {a}".to_string(), &[("a", "1"), ("b", "2")]), "2 then 1");
    }

    #[test]
    fn untranslated_text_passes_through() {
        assert_eq!(gettext_f("Page {page} of {total}", &[("page", "2"), ("total", "9")]), "Page 2 of 9");
    }
}
