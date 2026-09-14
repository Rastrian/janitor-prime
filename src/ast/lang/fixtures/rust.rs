// Bundled rust fixture corpus (E5 disposition): the fixed body of real-shape
// code the LangRegistry seeds its kind table from at first use — never
// "whichever file a scan hits first". Not compiled; parsed only.

use std::collections::HashMap;
use std::fmt::Display;

/// A documented unit exercising items, generics and traits.
#[derive(Debug, Clone, PartialEq)]
pub struct Widget<T: Display> {
    pub id: u32,
    /* block comment between fields */
    pub inner: T,
}

pub enum Kind {
    Alpha(u8),
    Beta { flag: bool },
    Gamma,
}

pub trait Inspect {
    fn inspect(&self) -> String;
    fn boxed(self: Box<Self>) -> Box<dyn Inspect>;
}

impl<T: Display> Inspect for Widget<T> {
    fn inspect(&self) -> String {
        format!("widget {} holding {}", self.id, self.inner)
    }

    fn boxed(self: Box<Self>) -> Box<dyn Inspect> {
        self
    }
}

impl<T: Display> Widget<T> {
    pub const MAX: usize = 64;

    pub fn new(id: u32, inner: T) -> Self {
        Self { id, inner }
    }

    pub fn classify(&self) -> Kind {
        match self.id {
            0 => Kind::Alpha(0),
            n if n % 2 == 0 => Kind::Beta { flag: true },
            _ => Kind::Gamma,
        }
    }

    pub fn tally(&self, rows: &HashMap<u32, u32>) -> Option<u32> {
        let mut acc: u32 = 0;
        for (_key, value) in rows.iter() {
            acc += *value;
        }
        while acc > Self::MAX as u32 {
            acc /= 2;
        }
        if acc == 0 {
            None
        } else {
            Some(acc)
        }
    }

    pub fn lazy(&self) -> impl Fn() -> u32 + '_ {
        move || self.id * 2
    }
}

static BANNER: &str = "widgetry";
const RADIX: u32 = 10;

pub fn main() {
    let mut rows = HashMap::new();
    rows.insert(1, 2);
    let w = Widget::new(7, 'x');
    let doubled = w.lazy()();
    let text = w.inspect();
    let _ = format!("{BANNER} {text} {doubled} {}", w.id as u64);
    let slice = &[1u8, 2, 3][..];
    let (a, .., b) = (1, 2, 3, 4);
    let _sum: u32 = slice.iter().map(|b| u32::from(*b)).sum();
    let _hex = format!("{:width$x}", 255, width = 4);
    let _parsed: u32 = "42".parse::<u32>().unwrap_or(RADIX);
    println!("{a}/{b} {} chars", text.len());
}
