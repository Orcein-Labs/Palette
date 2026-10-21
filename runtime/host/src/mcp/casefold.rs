//! Python's `str.casefold()`: Rust's per-character lowercase plus the
//! characters where full case folding differs from it (generated from
//! CPython 3.14, Unicode 16), so a path that folds to a protected name on
//! the Python host folds to it here too.

enum Fold {
    /// The character `n` code points on (0: the character itself).
    Shift(i32),
    Text(&'static str),
}

fn exception(c: char) -> Option<Fold> {
    match c {
        '\u{b5}' => Some(Fold::Shift(775)),
        '\u{17f}' => Some(Fold::Shift(-268)),
        '\u{345}' => Some(Fold::Shift(116)),
        '\u{3c2}' => Some(Fold::Shift(1)),
        '\u{3d0}' => Some(Fold::Shift(-30)),
        '\u{3d1}' => Some(Fold::Shift(-25)),
        '\u{3d5}' => Some(Fold::Shift(-15)),
        '\u{3d6}' => Some(Fold::Shift(-22)),
        '\u{3f0}' => Some(Fold::Shift(-54)),
        '\u{3f1}' => Some(Fold::Shift(-48)),
        '\u{3f5}' => Some(Fold::Shift(-64)),
        '\u{13a0}'..='\u{13f5}' => Some(Fold::Shift(0)),
        '\u{13f8}'..='\u{13fd}' => Some(Fold::Shift(-8)),
        '\u{1c80}' => Some(Fold::Shift(-6222)),
        '\u{1c81}' => Some(Fold::Shift(-6221)),
        '\u{1c82}' => Some(Fold::Shift(-6212)),
        '\u{1c83}'..='\u{1c84}' => Some(Fold::Shift(-6210)),
        '\u{1c85}' => Some(Fold::Shift(-6211)),
        '\u{1c86}' => Some(Fold::Shift(-6204)),
        '\u{1c87}' => Some(Fold::Shift(-6180)),
        '\u{1c88}' => Some(Fold::Shift(35267)),
        '\u{1e9b}' => Some(Fold::Shift(-58)),
        '\u{1fbe}' => Some(Fold::Shift(-7173)),
        '\u{a7ce}' => Some(Fold::Shift(0)),
        '\u{a7d2}' => Some(Fold::Shift(0)),
        '\u{a7d4}' => Some(Fold::Shift(0)),
        '\u{ab70}'..='\u{abbf}' => Some(Fold::Shift(-38864)),
        '\u{16ea0}'..='\u{16eb8}' => Some(Fold::Shift(0)),
        '\u{df}' => Some(Fold::Text("\u{73}\u{73}")),
        '\u{149}' => Some(Fold::Text("\u{2bc}\u{6e}")),
        '\u{1f0}' => Some(Fold::Text("\u{6a}\u{30c}")),
        '\u{390}' => Some(Fold::Text("\u{3b9}\u{308}\u{301}")),
        '\u{3b0}' => Some(Fold::Text("\u{3c5}\u{308}\u{301}")),
        '\u{587}' => Some(Fold::Text("\u{565}\u{582}")),
        '\u{1e96}' => Some(Fold::Text("\u{68}\u{331}")),
        '\u{1e97}' => Some(Fold::Text("\u{74}\u{308}")),
        '\u{1e98}' => Some(Fold::Text("\u{77}\u{30a}")),
        '\u{1e99}' => Some(Fold::Text("\u{79}\u{30a}")),
        '\u{1e9a}' => Some(Fold::Text("\u{61}\u{2be}")),
        '\u{1e9e}' => Some(Fold::Text("\u{73}\u{73}")),
        '\u{1f50}' => Some(Fold::Text("\u{3c5}\u{313}")),
        '\u{1f52}' => Some(Fold::Text("\u{3c5}\u{313}\u{300}")),
        '\u{1f54}' => Some(Fold::Text("\u{3c5}\u{313}\u{301}")),
        '\u{1f56}' => Some(Fold::Text("\u{3c5}\u{313}\u{342}")),
        '\u{1f80}' => Some(Fold::Text("\u{1f00}\u{3b9}")),
        '\u{1f81}' => Some(Fold::Text("\u{1f01}\u{3b9}")),
        '\u{1f82}' => Some(Fold::Text("\u{1f02}\u{3b9}")),
        '\u{1f83}' => Some(Fold::Text("\u{1f03}\u{3b9}")),
        '\u{1f84}' => Some(Fold::Text("\u{1f04}\u{3b9}")),
        '\u{1f85}' => Some(Fold::Text("\u{1f05}\u{3b9}")),
        '\u{1f86}' => Some(Fold::Text("\u{1f06}\u{3b9}")),
        '\u{1f87}' => Some(Fold::Text("\u{1f07}\u{3b9}")),
        '\u{1f88}' => Some(Fold::Text("\u{1f00}\u{3b9}")),
        '\u{1f89}' => Some(Fold::Text("\u{1f01}\u{3b9}")),
        '\u{1f8a}' => Some(Fold::Text("\u{1f02}\u{3b9}")),
        '\u{1f8b}' => Some(Fold::Text("\u{1f03}\u{3b9}")),
        '\u{1f8c}' => Some(Fold::Text("\u{1f04}\u{3b9}")),
        '\u{1f8d}' => Some(Fold::Text("\u{1f05}\u{3b9}")),
        '\u{1f8e}' => Some(Fold::Text("\u{1f06}\u{3b9}")),
        '\u{1f8f}' => Some(Fold::Text("\u{1f07}\u{3b9}")),
        '\u{1f90}' => Some(Fold::Text("\u{1f20}\u{3b9}")),
        '\u{1f91}' => Some(Fold::Text("\u{1f21}\u{3b9}")),
        '\u{1f92}' => Some(Fold::Text("\u{1f22}\u{3b9}")),
        '\u{1f93}' => Some(Fold::Text("\u{1f23}\u{3b9}")),
        '\u{1f94}' => Some(Fold::Text("\u{1f24}\u{3b9}")),
        '\u{1f95}' => Some(Fold::Text("\u{1f25}\u{3b9}")),
        '\u{1f96}' => Some(Fold::Text("\u{1f26}\u{3b9}")),
        '\u{1f97}' => Some(Fold::Text("\u{1f27}\u{3b9}")),
        '\u{1f98}' => Some(Fold::Text("\u{1f20}\u{3b9}")),
        '\u{1f99}' => Some(Fold::Text("\u{1f21}\u{3b9}")),
        '\u{1f9a}' => Some(Fold::Text("\u{1f22}\u{3b9}")),
        '\u{1f9b}' => Some(Fold::Text("\u{1f23}\u{3b9}")),
        '\u{1f9c}' => Some(Fold::Text("\u{1f24}\u{3b9}")),
        '\u{1f9d}' => Some(Fold::Text("\u{1f25}\u{3b9}")),
        '\u{1f9e}' => Some(Fold::Text("\u{1f26}\u{3b9}")),
        '\u{1f9f}' => Some(Fold::Text("\u{1f27}\u{3b9}")),
        '\u{1fa0}' => Some(Fold::Text("\u{1f60}\u{3b9}")),
        '\u{1fa1}' => Some(Fold::Text("\u{1f61}\u{3b9}")),
        '\u{1fa2}' => Some(Fold::Text("\u{1f62}\u{3b9}")),
        '\u{1fa3}' => Some(Fold::Text("\u{1f63}\u{3b9}")),
        '\u{1fa4}' => Some(Fold::Text("\u{1f64}\u{3b9}")),
        '\u{1fa5}' => Some(Fold::Text("\u{1f65}\u{3b9}")),
        '\u{1fa6}' => Some(Fold::Text("\u{1f66}\u{3b9}")),
        '\u{1fa7}' => Some(Fold::Text("\u{1f67}\u{3b9}")),
        '\u{1fa8}' => Some(Fold::Text("\u{1f60}\u{3b9}")),
        '\u{1fa9}' => Some(Fold::Text("\u{1f61}\u{3b9}")),
        '\u{1faa}' => Some(Fold::Text("\u{1f62}\u{3b9}")),
        '\u{1fab}' => Some(Fold::Text("\u{1f63}\u{3b9}")),
        '\u{1fac}' => Some(Fold::Text("\u{1f64}\u{3b9}")),
        '\u{1fad}' => Some(Fold::Text("\u{1f65}\u{3b9}")),
        '\u{1fae}' => Some(Fold::Text("\u{1f66}\u{3b9}")),
        '\u{1faf}' => Some(Fold::Text("\u{1f67}\u{3b9}")),
        '\u{1fb2}' => Some(Fold::Text("\u{1f70}\u{3b9}")),
        '\u{1fb3}' => Some(Fold::Text("\u{3b1}\u{3b9}")),
        '\u{1fb4}' => Some(Fold::Text("\u{3ac}\u{3b9}")),
        '\u{1fb6}' => Some(Fold::Text("\u{3b1}\u{342}")),
        '\u{1fb7}' => Some(Fold::Text("\u{3b1}\u{342}\u{3b9}")),
        '\u{1fbc}' => Some(Fold::Text("\u{3b1}\u{3b9}")),
        '\u{1fc2}' => Some(Fold::Text("\u{1f74}\u{3b9}")),
        '\u{1fc3}' => Some(Fold::Text("\u{3b7}\u{3b9}")),
        '\u{1fc4}' => Some(Fold::Text("\u{3ae}\u{3b9}")),
        '\u{1fc6}' => Some(Fold::Text("\u{3b7}\u{342}")),
        '\u{1fc7}' => Some(Fold::Text("\u{3b7}\u{342}\u{3b9}")),
        '\u{1fcc}' => Some(Fold::Text("\u{3b7}\u{3b9}")),
        '\u{1fd2}' => Some(Fold::Text("\u{3b9}\u{308}\u{300}")),
        '\u{1fd3}' => Some(Fold::Text("\u{3b9}\u{308}\u{301}")),
        '\u{1fd6}' => Some(Fold::Text("\u{3b9}\u{342}")),
        '\u{1fd7}' => Some(Fold::Text("\u{3b9}\u{308}\u{342}")),
        '\u{1fe2}' => Some(Fold::Text("\u{3c5}\u{308}\u{300}")),
        '\u{1fe3}' => Some(Fold::Text("\u{3c5}\u{308}\u{301}")),
        '\u{1fe4}' => Some(Fold::Text("\u{3c1}\u{313}")),
        '\u{1fe6}' => Some(Fold::Text("\u{3c5}\u{342}")),
        '\u{1fe7}' => Some(Fold::Text("\u{3c5}\u{308}\u{342}")),
        '\u{1ff2}' => Some(Fold::Text("\u{1f7c}\u{3b9}")),
        '\u{1ff3}' => Some(Fold::Text("\u{3c9}\u{3b9}")),
        '\u{1ff4}' => Some(Fold::Text("\u{3ce}\u{3b9}")),
        '\u{1ff6}' => Some(Fold::Text("\u{3c9}\u{342}")),
        '\u{1ff7}' => Some(Fold::Text("\u{3c9}\u{342}\u{3b9}")),
        '\u{1ffc}' => Some(Fold::Text("\u{3c9}\u{3b9}")),
        '\u{fb00}' => Some(Fold::Text("\u{66}\u{66}")),
        '\u{fb01}' => Some(Fold::Text("\u{66}\u{69}")),
        '\u{fb02}' => Some(Fold::Text("\u{66}\u{6c}")),
        '\u{fb03}' => Some(Fold::Text("\u{66}\u{66}\u{69}")),
        '\u{fb04}' => Some(Fold::Text("\u{66}\u{66}\u{6c}")),
        '\u{fb05}' => Some(Fold::Text("\u{73}\u{74}")),
        '\u{fb06}' => Some(Fold::Text("\u{73}\u{74}")),
        '\u{fb13}' => Some(Fold::Text("\u{574}\u{576}")),
        '\u{fb14}' => Some(Fold::Text("\u{574}\u{565}")),
        '\u{fb15}' => Some(Fold::Text("\u{574}\u{56b}")),
        '\u{fb16}' => Some(Fold::Text("\u{57e}\u{576}")),
        '\u{fb17}' => Some(Fold::Text("\u{574}\u{56d}")),
        _ => None,
    }
}

pub(crate) fn casefold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match exception(c) {
            Some(Fold::Shift(n)) => out.push((c as u32).checked_add_signed(n).and_then(char::from_u32).expect("the table shifts to a character")),
            Some(Fold::Text(t)) => out.push_str(t),
            None => out.extend(c.to_lowercase()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::casefold;

    // Expected values are what CPython 3.14 prints for `s.casefold()`.
    #[test]
    fn folds_as_python_folds() {
        for (s, folded) in [
            ("Secrets", "secrets"),
            ("\u{17f}ecret", "secret"),
            ("Stra\u{df}e", "strasse"),
            ("\u{1e9e}", "ss"),
            ("\u{fb01}le", "file"),
            ("\u{3a3}\u{3c2}", "\u{3c3}\u{3c3}"),
            ("\u{13a0}\u{ab70}", "\u{13a0}\u{13a0}"),
            ("\u{1f88}", "\u{1f00}\u{3b9}"),
            ("\u{212a}", "k"),
            ("\u{130}", "i\u{307}"),
            ("CREDENTIAL\u{fb05}", "credentialst"),
        ] {
            assert_eq!(casefold(s), folded, "{s:?}");
        }
    }
}
