#![allow(dead_code)]
//! Valid Rust that an edit may write: macros, attributes and recent syntax.

use std::collections::HashMap;

macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
    ($($x:expr),+ $(,)?) => {
        [$(square!($x)),+]
    };
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(test, derive(Default))]
pub struct Point<const N: usize> {
    coords: [f64; N],
}

pub trait Store {
    type Item<'a>
    where
        Self: 'a;
    async fn load(&self) -> Option<String>;
    fn get<'a>(&'a self) -> Self::Item<'a>;
}

pub fn chains(a: Option<u8>, b: bool, map: &HashMap<String, u8>) -> u8 {
    if let Some(x) = a
        && b
        && let Some(y) = map.get("k")
    {
        return x + y;
    }
    let Some(z) = a else { return 0 };
    let n = 'outer: {
        if z > 3 {
            break 'outer 1;
        }
        2
    };
    let c = c"bytes";
    let r = r#"raw "quoted" text"#;
    let raw = &raw const n;
    let _ = (c, r, raw, const { 1 + 2 });
    let add = async move |v: u8| v + 1;
    let _ = add;
    println!("{n} {}", square!(3));
    format!("{z:>width$}", width = 4).len() as u8
}

pub fn captures<'a>(text: &'a str) -> impl Iterator<Item = char> + use<'a> {
    text.chars()
}

unsafe extern "C" {
    pub safe fn abs(input: i32) -> i32;
}

#[cfg(test)]
mod tests {
    #[test]
    fn works() {
        assert_eq!(super::chains(None, true, &Default::default()), 0);
    }
}
