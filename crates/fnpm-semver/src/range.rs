//! [`Range`], [`Comparator`] and the npm range parser.
//!
//! Port of bun `semver/SemverQuery.rs` (`parse_with`, `Token::to_range`) and
//! `semver/SemverRange.rs` (`Comparator::satisfies`, the `lt_next_*` / `gte_next_*` bounds).
//! bun stores a range as linked lists of `left`/`right` comparator pairs; here every pair is
//! flattened into one comparator set, which is equivalent for every query bun makes.

use std::fmt;

use crate::scan::{self, Scan, Wildcard};
use crate::version::{Identifier, Version, ascii, identifiers};
use crate::{Error, Result};

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    /// `=` (also a bare version).
    Eq,
    /// `<`.
    Lt,
    /// `<=`.
    Lte,
    /// `>`.
    Gt,
    /// `>=`.
    Gte,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "",
            Op::Lt => "<",
            Op::Lte => "<=",
            Op::Gt => ">",
            Op::Gte => ">=",
        }
    }
}

/// One `op version` test.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Comparator {
    /// The operator.
    pub op: Op,
    /// The version compared against.
    pub version: Version,
}

impl Comparator {
    /// `op version`.
    pub fn new(op: Op, version: Version) -> Self {
        Self { op, version }
    }

    /// Whether `v` passes this comparator on its own (no prerelease rule). Build metadata is
    /// ignored. Like node-semver, `>=0.0.0` also admits prereleases of `0.0.0`.
    #[inline]
    pub fn matches(&self, v: &Version) -> bool {
        use std::cmp::Ordering;
        match v.cmp_precedence(&self.version) {
            Ordering::Equal => matches!(self.op, Op::Lte | Op::Gte | Op::Eq),
            Ordering::Greater => matches!(self.op, Op::Gt | Op::Gte),
            Ordering::Less => match self.op {
                Op::Gte => self.version.is_zero() && !self.version.is_prerelease(),
                op => matches!(op, Op::Lt | Op::Lte),
            },
        }
    }

    /// `<0.0.0-0`, which no version satisfies (node-semver's empty set).
    pub(crate) fn null_set() -> Self {
        Self::new(Op::Lt, ver(0, 0, 0, Tag::ZERO))
    }
}

impl fmt::Display for Comparator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.op.as_str(), self.version)
    }
}

/// A range: `||`-separated comparator sets, each an AND of comparators. `""`, and text with no
/// comparator at all, is the single empty set (any release).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Range {
    /// The alternatives; a version satisfies the range when it satisfies one of them.
    pub sets: Vec<Vec<Comparator>>,
}

/// Options for [`Range::satisfies_with`] and [`crate::max_satisfying`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SatisfyOptions {
    /// npm `includePrerelease`: a prerelease only has to pass the comparators. For node-semver's
    /// exact semantics (where `1.x` also starts at `1.0.0-0`) parse the range with
    /// [`Range::parse_including_prerelease`].
    pub include_prerelease: bool,
}

/// What the lenient parser noticed while reading a range (bun `query::Flags`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParseInfo {
    /// Some written version has a prerelease (bun `Flags::PRE`). The resolver then also looks
    /// at prereleases and only prefers `latest` when the first comparator is exactly it.
    pub has_prerelease: bool,
    /// Some written version has build metadata (bun `Flags::BUILD`).
    pub has_build: bool,
    /// The parser skipped text that is not a comparator (`latest`, `>=1 && <2`, `1.2.3.4`).
    /// npm refuses such a range.
    pub has_skipped: bool,
}

// ── internal tag model used while desugaring ─────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Pre<'a> {
    Raw(&'a [u8]),
    /// `-0`, the lowest prerelease.
    Zero,
}

#[derive(Clone, Copy)]
struct Tag<'a> {
    pre: Pre<'a>,
    build: &'a [u8],
}

impl<'a> Tag<'a> {
    const EMPTY: Tag<'static> = Tag {
        pre: Pre::Raw(b""),
        build: b"",
    };
    const ZERO: Tag<'static> = Tag {
        pre: Pre::Zero,
        build: b"",
    };

    fn of(raw: scan::RawTag<'a>) -> Self {
        Tag {
            pre: Pre::Raw(raw.pre),
            build: raw.build,
        }
    }

    fn has_pre(&self) -> bool {
        match self.pre {
            Pre::Raw(p) => !p.is_empty(),
            Pre::Zero => true,
        }
    }
}

fn ver(major: u64, minor: u64, patch: u64, tag: Tag<'_>) -> Version {
    Version {
        major,
        minor,
        patch,
        pre: match tag.pre {
            Pre::Raw(p) => identifiers(p),
            Pre::Zero => vec![Identifier::Numeric(0)],
        },
        build: ascii(tag.build).into(),
    }
}

fn cmp(op: Op, major: u64, minor: u64, patch: u64, tag: Tag<'_>) -> Comparator {
    Comparator::new(op, ver(major, minor, patch, tag))
}

/// `<{major+1}.0.0-0`, or `<=MAX.MAX.MAX` when `major+1` overflows.
fn lt_next_major(major: u64) -> Comparator {
    match major.checked_add(1) {
        Some(m) => cmp(Op::Lt, m, 0, 0, Tag::ZERO),
        None => cmp(Op::Lte, u64::MAX, u64::MAX, u64::MAX, Tag::EMPTY),
    }
}

/// `<{major}.{minor+1}.0-0`, or `<={major}.MAX.MAX` on overflow.
fn lt_next_minor(major: u64, minor: u64) -> Comparator {
    match minor.checked_add(1) {
        Some(m) => cmp(Op::Lt, major, m, 0, Tag::ZERO),
        None => cmp(Op::Lte, major, u64::MAX, u64::MAX, Tag::EMPTY),
    }
}

/// `<{major}.{minor}.{patch+1}-0`, or `<={major}.{minor}.MAX` on overflow.
fn lt_next_patch(major: u64, minor: u64, patch: u64) -> Comparator {
    match patch.checked_add(1) {
        Some(p) => cmp(Op::Lt, major, minor, p, Tag::ZERO),
        None => cmp(Op::Lte, major, minor, u64::MAX, Tag::EMPTY),
    }
}

/// node-semver's `>{major}`: `>={major+1}.0.0`; nothing on overflow.
fn gte_next_major(major: u64, tag: Tag<'_>) -> Comparator {
    match major.checked_add(1) {
        Some(m) => cmp(Op::Gte, m, 0, 0, tag),
        None => Comparator::null_set(),
    }
}

/// node-semver's `>{major}.{minor}`: `>={major}.{minor+1}.0`, or the next major on overflow.
fn gte_next_minor(major: u64, minor: u64, tag: Tag<'_>) -> Comparator {
    match minor.checked_add(1) {
        Some(m) => cmp(Op::Gte, major, m, 0, tag),
        None => gte_next_major(major, tag),
    }
}

/// bun's `Range`: up to two comparators that are AND-ed. `None`/`None` adds no constraint.
#[derive(Default)]
struct Pair {
    left: Option<Comparator>,
    right: Option<Comparator>,
}

impl Pair {
    fn one(c: Comparator) -> Self {
        Pair {
            left: Some(c),
            right: None,
        }
    }
    fn two(left: Comparator, right: Comparator) -> Self {
        Pair {
            left: Some(left),
            right: Some(right),
        }
    }
    fn into_iter(self) -> impl Iterator<Item = Comparator> {
        self.left.into_iter().chain(self.right)
    }
}

fn init_wildcard(p: &scan::Partial<'_>, wildcard: Wildcard, floor: Tag<'_>) -> Pair {
    let (major, minor) = (p.major.unwrap_or(0), p.minor.unwrap_or(0));
    match wildcard {
        Wildcard::None => Pair::one(Comparator::new(Op::Eq, Version::from_partial(p))),
        Wildcard::Major => Pair::one(cmp(Op::Gte, 0, 0, 0, floor)),
        Wildcard::Minor => Pair::two(lt_next_major(major), cmp(Op::Gte, major, 0, 0, floor)),
        Wildcard::Patch => Pair::two(
            lt_next_minor(major, minor),
            cmp(Op::Gte, major, minor, 0, floor),
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum TokenTag {
    #[default]
    None,
    Gt,
    Gte,
    Lt,
    Lte,
    Version,
    Tilde,
    Caret,
}

/// bun `Token::to_range`: desugars one operator + partial version.
fn to_range(tag: TokenTag, wildcard: Wildcard, parsed: &Scan<'_>, floor: Tag<'_>) -> Pair {
    let v = &parsed.version;
    // Only read for `Wildcard::Major`, which is valid only when it is a written `x`, `X` or `*`.
    let wildcard_is_written = parsed.valid;
    let vtag = Tag::of(v.tag);
    match tag {
        TokenTag::Caret => {
            let Some(major) = v.major else {
                return if wildcard_is_written {
                    init_wildcard(v, Wildcard::Major, floor)
                } else {
                    Pair::default()
                };
            };
            if let Some(minor) = v.minor {
                if let Some(patch) = v.patch {
                    if major == 0 {
                        let mut left_tag = vtag;
                        if !vtag.has_pre() {
                            left_tag.pre = floor.pre;
                        }
                        let right = if minor == 0 {
                            lt_next_patch(0, 0, patch)
                        } else {
                            lt_next_minor(0, minor)
                        };
                        return Pair::two(cmp(Op::Gte, 0, minor, patch, left_tag), right);
                    }
                    return Pair::two(
                        cmp(Op::Gte, major, minor, patch, vtag),
                        lt_next_major(major),
                    );
                }
                if major == 0 {
                    return Pair::two(cmp(Op::Gte, 0, minor, 0, floor), lt_next_minor(0, minor));
                }
                return Pair::two(cmp(Op::Gte, major, minor, 0, floor), lt_next_major(major));
            }
            Pair::two(cmp(Op::Gte, major, 0, 0, floor), lt_next_major(major))
        }
        TokenTag::Tilde => {
            let Some(major) = v.major else {
                return if wildcard_is_written {
                    init_wildcard(v, Wildcard::Major, floor)
                } else {
                    Pair::default()
                };
            };
            if let Some(minor) = v.minor {
                let left = match v.patch {
                    Some(patch) => cmp(Op::Gte, major, minor, patch, vtag),
                    None => cmp(Op::Gte, major, minor, 0, Tag::EMPTY),
                };
                return Pair::two(left, lt_next_minor(major, minor));
            }
            Pair::two(cmp(Op::Gte, major, 0, 0, Tag::EMPTY), lt_next_major(major))
        }
        TokenTag::Version => {
            if wildcard != Wildcard::None {
                init_wildcard(v, wildcard, floor)
            } else {
                Pair::one(Comparator::new(Op::Eq, Version::from_partial(v)))
            }
        }
        TokenTag::None => Pair::default(),
        TokenTag::Gt | TokenTag::Gte | TokenTag::Lt | TokenTag::Lte => {
            // node-semver's `replaceXRange`
            let op = match tag {
                TokenTag::Gt => Op::Gt,
                TokenTag::Gte => Op::Gte,
                TokenTag::Lt => Op::Lt,
                _ => Op::Lte,
            };
            let (major, minor) = (v.major.unwrap_or(0), v.minor.unwrap_or(0));
            let left = match (op, wildcard) {
                (_, Wildcard::None) => Comparator::new(op, Version::from_partial(v)),
                (Op::Gt | Op::Lt, Wildcard::Major) if wildcard_is_written => Comparator::null_set(),
                // An operator before text that is not a version (`>latest`) also gets here.
                (_, Wildcard::Major) => return init_wildcard(v, Wildcard::Major, floor),
                (Op::Gte, _) => cmp(op, major, minor, 0, floor),
                (Op::Lt, _) => cmp(op, major, minor, 0, Tag::ZERO),
                (Op::Gt, Wildcard::Minor) => gte_next_major(major, floor),
                (Op::Gt, _) => gte_next_minor(major, minor, floor),
                (_, Wildcard::Minor) => lt_next_major(major),
                (_, _) => lt_next_minor(major, minor),
            };
            Pair::one(left)
        }
    }
}

/// Accumulates comparator sets with bun `Group::and_range` / `Group::or_range` semantics.
struct Builder {
    sets: Vec<Vec<Comparator>>,
}

impl Builder {
    fn and(&mut self, pair: Pair) {
        if let Some(last) = self.sets.last_mut() {
            last.extend(pair.into_iter());
        }
    }

    fn or(&mut self, pair: Pair) {
        if self.sets.len() == 1 && self.sets[0].is_empty() {
            self.sets[0].extend(pair.into_iter());
        } else {
            let mut set = Vec::with_capacity(2);
            set.extend(pair.into_iter());
            self.sets.push(set);
        }
    }
}

#[inline]
fn skip_spaces(input: &[u8], i: &mut usize) {
    while *i < input.len() && input[*i] == b' ' {
        *i += 1;
    }
}

/// bun `query::parse_with`. Never fails: unknown text is skipped and reported in `ParseInfo`.
fn parse_with(input: &[u8], floor: Tag<'static>) -> (Range, ParseInfo) {
    let null_set = |info: ParseInfo| {
        (
            Range {
                sets: vec![vec![Comparator::null_set()]],
            },
            info,
        )
    };
    let len = input.len();
    let mut info = ParseInfo::default();
    let mut out = Builder {
        sets: vec![Vec::new()],
    };
    let mut i = 0usize;
    let mut token_tag = TokenTag::None;
    let mut is_or = false;
    // Only spaces lie between the cursor and the last comparator, and that comparator is valid.
    let mut after_comparator = false;

    while i < len {
        let mut skip_round = false;
        let mut no_operator = false;
        match input[i] {
            c @ (b'>' | b'<') => {
                i += 1;
                // "< =1" is "<=1"
                skip_spaces(input, &mut i);
                let or_equal = i < len && input[i] == b'=';
                i += usize::from(or_equal);
                token_tag = match (c, or_equal) {
                    (b'>', false) => TokenTag::Gt,
                    (b'>', true) => TokenTag::Gte,
                    (_, false) => TokenTag::Lt,
                    (_, true) => TokenTag::Lte,
                };
                skip_spaces(input, &mut i);
            }
            b'=' | b'v' => {
                token_tag = TokenTag::Version;
                no_operator = true;
                i += 1;
                skip_spaces(input, &mut i);
            }
            b'~' => {
                token_tag = TokenTag::Tilde;
                i += 1;
                if i < len && input[i] == b'>' {
                    i += 1;
                }
                skip_spaces(input, &mut i);
            }
            b'^' => {
                token_tag = TokenTag::Caret;
                i += 1;
                skip_spaces(input, &mut i);
            }
            b'0'..=b'9' | b'X' | b'x' | b'*' => {
                token_tag = TokenTag::Version;
                no_operator = true;
            }
            b'|' => {
                i += 1;
                while i < len && input[i] == b'|' {
                    i += 1;
                }
                skip_spaces(input, &mut i);
                is_or = true;
                token_tag = TokenTag::None;
                skip_round = true;
            }
            b'-' => {
                i += 1;
                skip_spaces(input, &mut i);
            }
            b' ' => {
                i += 1;
                skip_spaces(input, &mut i);
                skip_round = true;
            }
            _ => {
                // A tagged version like "boop" in "1.0.0 || boop" is skipped.
                i += 1;
                token_tag = TokenTag::None;
                after_comparator = false;
                while i < len && input[i] != b' ' && input[i] != b'|' {
                    i += 1;
                }
                skip_round = true;
                info.has_skipped = true;
            }
        }
        if skip_round {
            continue;
        }

        let parsed = scan::scan_version(&input[i..]);
        // node-semver rejects the whole range for a number it cannot hold.
        if parsed.overflow {
            return null_set(info);
        }
        note(&mut info, &parsed);
        let wildcard = parsed.wildcard;
        i += parsed.len;
        let rollback = i;

        let maybe_hyphenate = i < len && (input[i] == b' ' || input[i] == b'-');
        let hyphenate = maybe_hyphenate && {
            i += scan::leading_ws(&input[i..]);
            if !(i < len && input[i] == b'-') {
                false
            } else {
                i += 1;
                i += scan::leading_ws(&input[i..]);
                if i == len {
                    false
                } else {
                    if input[i] == b'v' || input[i] == b'=' {
                        i += 1;
                    }
                    if i == len {
                        false
                    } else {
                        i += scan::leading_ws(&input[i..]);
                        i != len && matches!(input[i], b'0'..=b'9' | b'X' | b'x' | b'*')
                    }
                }
            }
        };
        if !hyphenate {
            i = rollback;
        }
        i += usize::from(!hyphenate);

        let valid = parsed.valid && !hyphenate;
        // Malformed text ("=>16.0.0", "1.2.3.4") starts an alternative so it cannot narrow the range.
        if no_operator && !(after_comparator && valid) {
            is_or = true;
        }

        let pair = if hyphenate {
            let second = scan::scan_version(input.get(i..).unwrap_or_default());
            if second.overflow {
                return null_set(info);
            }
            note(&mut info, &second);
            i += second.len + 1;
            let first = &parsed.version;
            let mut first_tag = Tag::of(first.tag);
            if !first_tag.has_pre() {
                first_tag.pre = floor.pre;
            }
            let left = cmp(
                Op::Gte,
                first.major.unwrap_or(0),
                first.minor.unwrap_or(0),
                first.patch.unwrap_or(0),
                first_tag,
            );
            let s = &second.version;
            let (major, minor, patch) = (
                s.major.unwrap_or(0),
                s.minor.unwrap_or(0),
                s.patch.unwrap_or(0),
            );
            let right = match second.wildcard {
                // "1.0.0 - x" --> ">=1.0.0"
                Wildcard::Major => None,
                // "1.0.0 - 1.x" --> ">=1.0.0 <2.0.0-0"
                Wildcard::Minor => Some(lt_next_major(major)),
                // "1.0.0 - 1.0.x" --> ">=1.0.0 <1.1.0-0"
                Wildcard::Patch => Some(lt_next_minor(major, minor)),
                Wildcard::None if floor.has_pre() && s.tag.pre.is_empty() => {
                    Some(lt_next_patch(major, minor, patch))
                }
                Wildcard::None => Some(Comparator::new(Op::Lte, Version::from_partial(s))),
            };
            Pair {
                left: Some(left),
                right,
            }
        } else if token_tag == TokenTag::None {
            // No pending operator for this chunk ("--foo", or a dangling "-" after a skipped
            // tag): skip it like "boop" in "1.0.0 || boop", keeping any pending "||".
            info.has_skipped = true;
            continue;
        } else {
            to_range(token_tag, wildcard, &parsed, floor)
        };

        if is_or {
            out.or(pair);
        } else {
            out.and(pair);
        }
        is_or = false;
        after_comparator = valid && input.get(rollback) == Some(&b' ');
    }

    (Range { sets: out.sets }, info)
}

fn note(info: &mut ParseInfo, parsed: &Scan<'_>) {
    if !parsed.valid {
        info.has_skipped = true;
    }
    if !parsed.version.tag.build.is_empty() {
        info.has_build = true;
    }
    if !parsed.version.tag.pre.is_empty() {
        info.has_prerelease = true;
    }
}

/// Tests one comparator set against `v`.
#[inline]
pub(crate) fn set_satisfies(set: &[Comparator], v: &Version, include_prerelease: bool) -> bool {
    if v.pre.is_empty() || include_prerelease {
        return set.iter().all(|c| c.matches(v));
    }
    // A prerelease must also share `major.minor.patch` with a comparator that has a prerelease.
    // https://github.com/npm/node-semver/blob/ac9b35769ab0ddfefd5a3af4a3ecaf3da2012352/classes/range.js#L505
    let mut pre_matched = false;
    for c in set {
        if !c.matches(v) {
            return false;
        }
        pre_matched = pre_matched || (c.version.is_prerelease() && c.version.same_core(v));
    }
    pre_matched
}

impl Range {
    /// Parses any npm range the way bun does: `^1.2`, `~1`, `~>1`, `1.x`, `>=1 <2`, `1 - 2`,
    /// `1 || 2`, `*`, `""`, `v1`, `=1.2.3`, `< =1` and loose versions like `1.0.0rc1`.
    ///
    /// The parser is lenient and never fails: text that is not a comparator (`latest`, `&&`,
    /// `1.2.3.4`) is skipped, so a range of only such text matches every release. Use
    /// [`Range::parse_with_info`] to learn what was skipped, or [`Range::parse_strict`] to refuse
    /// it. A number above `u64::MAX` makes the whole range match nothing, as in node-semver.
    /// Dist tags are not ranges; callers resolve them first.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(parse_with(s.as_bytes(), Tag::EMPTY).0)
    }

    /// [`Range::parse`] plus what the parser noticed.
    pub fn parse_with_info(s: &str) -> (Self, ParseInfo) {
        parse_with(s.as_bytes(), Tag::EMPTY)
    }

    /// Like [`Range::parse`], but fails with [`Error::InvalidRange`] when the parser had to skip
    /// text that npm would refuse (`latest`, `>=1 && <2`, `16-18`, `1.2.3.4`).
    pub fn parse_strict(s: &str) -> Result<Self> {
        let (range, info) = Self::parse_with_info(s);
        if info.has_skipped {
            return Err(Error::InvalidRange(s.to_owned()));
        }
        Ok(range)
    }

    /// Parses with node-semver's `includePrerelease` desugaring (bun
    /// `parse_including_prerelease`): derived lower bounds start at `-0`, so `1.x` is
    /// `>=1.0.0-0 <2.0.0-0` and `>1` is `>=2.0.0-0`. Test with
    /// `satisfies_with(.., SatisfyOptions { include_prerelease: true })`.
    pub fn parse_including_prerelease(s: &str) -> Result<Self> {
        Ok(parse_with(s.as_bytes(), Tag::ZERO).0)
    }

    /// The range that every release satisfies (`*` / `""`).
    pub fn any() -> Self {
        Self {
            sets: vec![Vec::new()],
        }
    }

    /// Whether some alternative has no comparator, i.e. every release satisfies the range.
    pub fn is_any(&self) -> bool {
        self.sets.iter().any(Vec::is_empty)
    }

    /// bun `Group::is_empty`: the parser found no comparator at all (`""`, `latest`).
    pub fn has_no_comparators(&self) -> bool {
        matches!(self.sets.as_slice(), [set] if set.is_empty())
    }

    /// `Some` when the range is one exact version (`1.2.3`, `=1.2.3`): the resolver's fast path.
    pub fn as_exact(&self) -> Option<&Version> {
        match self.sets.as_slice() {
            [set] => match set.as_slice() {
                [
                    Comparator {
                        op: Op::Eq,
                        version,
                    },
                ] => Some(version),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether some comparator names a prerelease, apart from the `<X.Y.Z-0` upper bounds that
    /// `^`, `~`, x-ranges and hyphen ranges derive. This is bun `Flags::PRE` recovered from
    /// the comparators; it differs only for a written `<X.Y.Z-0` and for prereleases written on
    /// partial versions (`^1.2-beta`). [`Range::parse_with_info`] gives the exact flag.
    pub fn has_prerelease(&self) -> bool {
        self.sets.iter().flatten().any(|c| {
            c.version.is_prerelease()
                && !(c.op == Op::Lt && matches!(c.version.pre.as_slice(), [Identifier::Numeric(0)]))
        })
    }

    /// Whether `v` satisfies the range (npm `satisfies` without options).
    #[inline]
    pub fn satisfies(&self, v: &Version) -> bool {
        self.satisfies_with(v, SatisfyOptions::default())
    }

    /// Whether `v` satisfies the range. Allocation-free.
    #[inline]
    pub fn satisfies_with(&self, v: &Version, opts: SatisfyOptions) -> bool {
        self.sets
            .iter()
            .any(|set| set_satisfies(set, v, opts.include_prerelease))
    }

    /// The lowest version that can satisfy the range (node-semver `minVersion`), or `None` when
    /// nothing does.
    pub fn min_version(&self) -> Option<Version> {
        let zero = Version::new(0, 0, 0);
        if self.satisfies(&zero) {
            return Some(zero);
        }
        let zero_pre = Version {
            pre: vec![Identifier::Numeric(0)],
            ..Version::new(0, 0, 0)
        };
        if self.satisfies(&zero_pre) {
            return Some(zero_pre);
        }
        let mut min: Option<Version> = None;
        for set in &self.sets {
            let mut set_min: Option<Version> = None;
            for c in set {
                let candidate = match c.op {
                    Op::Gt => {
                        let mut v = c.version.clone();
                        v.build = Box::default();
                        if v.pre.is_empty() {
                            v.patch = v.patch.saturating_add(1);
                        } else {
                            v.pre.push(Identifier::Numeric(0));
                        }
                        v
                    }
                    Op::Eq | Op::Gte => {
                        let mut v = c.version.clone();
                        v.build = Box::default();
                        v
                    }
                    Op::Lt | Op::Lte => continue,
                };
                if set_min
                    .as_ref()
                    .is_none_or(|m| candidate.cmp_precedence(m).is_gt())
                {
                    set_min = Some(candidate);
                }
            }
            if let Some(set_min) = set_min
                && min
                    .as_ref()
                    .is_none_or(|m| m.cmp_precedence(&set_min).is_gt())
            {
                min = Some(set_min);
            }
        }
        min.filter(|m| self.satisfies(m))
    }

    /// The highest version that might satisfy the range, or `None` when unbounded. Every
    /// satisfying version is `<=` the returned bound (`<` when the `bool` is false).
    pub(crate) fn upper_bound(&self) -> Option<(&Version, bool)> {
        let mut best: Option<(&Version, bool)> = None;
        for set in &self.sets {
            let mut set_upper: Option<(&Version, bool)> = None;
            for c in set {
                let bound = match c.op {
                    Op::Lt => (&c.version, false),
                    Op::Lte | Op::Eq => (&c.version, true),
                    Op::Gt | Op::Gte => continue,
                };
                if set_upper.is_none_or(|u| bound.0.cmp_precedence(u.0).is_lt()) {
                    set_upper = Some(bound);
                }
            }
            let set_upper = set_upper?;
            if best.is_none_or(|b| set_upper.0.cmp_precedence(b.0).is_gt()) {
                best = Some(set_upper);
            }
        }
        best
    }
}

impl fmt::Display for Range {
    /// The normalized form: comparators joined by a space, alternatives by ` || `, an empty
    /// set as `*`, an exact comparator as `=x.y.z`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, set) in self.sets.iter().enumerate() {
            if i > 0 {
                f.write_str(" || ")?;
            }
            if set.is_empty() {
                f.write_str("*")?;
            }
            for (j, c) in set.iter().enumerate() {
                if j > 0 {
                    f.write_str(" ")?;
                }
                if c.op == Op::Eq {
                    f.write_str("=")?;
                }
                fmt::Display::fmt(c, f)?;
            }
        }
        Ok(())
    }
}

impl std::str::FromStr for Range {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}
