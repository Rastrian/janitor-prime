//! The ONLY file permitted to import `tree-sitter` types (PLAN §2, ADR-0015,
//! risk R22): a tree-sitter core pin-swap touches this file alone.
//!
//! F3 will implement, per PLAN §1.2 and §3 F3.1:
//!
//! - grammar registration via `tree_sitter_rust::LANGUAGE.into()`
//!   (`LanguageFn` — the current constructor; the old `language()` fn no
//!   longer exists, A.3.7);
//! - the `parse_with_options` wrapper whose callback serves chunks from an
//!   `Arc<[u8]>` overlay snapshot (no lock held during the parse, Rule T2);
//! - the `InputEdit` choreography helpers with byte-column `Point` math
//!   (A.3.4), behind the exclusivity guard of §1.2.1 step 5.
//!
//! F0 keeps this a declared, empty boundary so the fence is visible in the
//! tree from the first code commit. No `unsafe` will ever live in janitor
//! code (Rule T6).
// TODO(F3): parse_with_options wrapper, LanguageFn registration, InputEdit
// byte-column math (PLAN §1.2, §3 F3.1).
