//! Hand-rolled arbitrary-precision decimal for the SQL `numeric`/`decimal` type.
//!
//! PostgreSQL's `numeric` is an exact base-10 number with arbitrary length and a
//! tracked *scale* (number of fractional digits). We mirror that here with a
//! sign, an arbitrary-length vector of base-10 digits, and a scale. The digit
//! vector stores the *unscaled* integer magnitude, most-significant digit first
//! (e.g. value `12.340` is digits `[1,2,3,4,0]`, scale `3`).
//!
//! Arithmetic is performed on the unscaled integer magnitudes after aligning
//! scales, so results are exact. Division rounds half-up to a target scale, the
//! way PostgreSQL picks a result scale for the `/` operator.
//!
//! Everything is `std`-only.

use std::cmp::Ordering;
use std::fmt;

/// Maximum number of fractional digits produced by division before rounding.
/// PostgreSQL's `numeric` division selects a scale and rounds; we use a fixed,
/// generous default that comfortably covers the documented behavior.
const DIV_SCALE: usize = 16;

/// An exact, arbitrary-precision base-10 decimal.
///
/// Invariants (maintained by [`BigDecimal::normalize`] / the constructors):
/// - `digits` holds the unscaled magnitude, most-significant first, with no
///   leading zeros except the single digit `0` representing magnitude zero.
/// - `scale` is the number of those digits that are fractional.
/// - `negative` is never `true` for a zero magnitude (zero is canonical-positive).
#[derive(Clone, Debug)]
pub struct BigDecimal {
    negative: bool,
    /// Unscaled magnitude, big-endian decimal digits (each 0..=9).
    digits: Vec<u8>,
    /// Number of fractional digits (digits to the right of the decimal point).
    scale: usize,
}

impl BigDecimal {
    /// The value zero with scale 0.
    pub fn zero() -> BigDecimal {
        BigDecimal {
            negative: false,
            digits: vec![0],
            scale: 0,
        }
    }

    /// Exact conversion from an `i64` (scale 0).
    pub fn from_i64(mut v: i64) -> BigDecimal {
        if v == 0 {
            return BigDecimal::zero();
        }
        let negative = v < 0;
        // Build digits least-significant-first, handling i64::MIN without overflow.
        let mut rev = Vec::new();
        // Work in i128 to avoid overflow on negation of i64::MIN.
        let mut n = (v as i128).unsigned_abs();
        let _ = &mut v;
        while n > 0 {
            rev.push((n % 10) as u8);
            n /= 10;
        }
        rev.reverse();
        let mut d = BigDecimal {
            negative,
            digits: rev,
            scale: 0,
        };
        d.normalize();
        d
    }

    /// Parse a decimal string such as `-123.4500`, `+12`, `.5`, `1e3`.
    ///
    /// Accepts an optional leading sign, decimal point, and `eN` exponent. The
    /// scale of the result reflects the literal text (trailing zeros are kept),
    /// adjusted by any exponent. Returns `None` for malformed input.
    pub fn parse(input: &str) -> Option<BigDecimal> {
        let s = input.trim();
        if s.is_empty() {
            return None;
        }
        let mut chars = s.chars().peekable();
        let mut negative = false;
        match chars.peek() {
            Some('+') => {
                chars.next();
            }
            Some('-') => {
                negative = true;
                chars.next();
            }
            _ => {}
        }

        let mut digits: Vec<u8> = Vec::new();
        let mut frac_digits: usize = 0;
        let mut seen_dot = false;
        let mut seen_any = false;
        let mut exp: i64 = 0;

        while let Some(&c) = chars.peek() {
            match c {
                '0'..='9' => {
                    digits.push((c as u8) - b'0');
                    if seen_dot {
                        frac_digits += 1;
                    }
                    seen_any = true;
                    chars.next();
                }
                '.' => {
                    if seen_dot {
                        return None;
                    }
                    seen_dot = true;
                    chars.next();
                }
                'e' | 'E' => {
                    chars.next();
                    let mut exp_str = String::new();
                    if let Some(&sign) = chars.peek()
                        && (sign == '+' || sign == '-')
                    {
                        exp_str.push(sign);
                        chars.next();
                    }
                    let mut any_exp = false;
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() {
                            exp_str.push(d);
                            any_exp = true;
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if !any_exp {
                        return None;
                    }
                    exp = exp_str.parse::<i64>().ok()?;
                    break;
                }
                _ => return None,
            }
        }
        // Anything left over (after a valid exponent) is invalid.
        if chars.peek().is_some() {
            return None;
        }
        if !seen_any {
            return None;
        }

        // Apply the exponent by shifting the decimal point: a positive exponent
        // reduces fractional digits (padding with zeros if needed); a negative
        // one increases them.
        let mut scale = frac_digits as i64 - exp;
        if scale < 0 {
            // Need to append `-scale` zeros to the integer part.
            digits.resize(digits.len() + (-scale) as usize, 0);
            scale = 0;
        }
        let mut d = BigDecimal {
            negative,
            digits,
            scale: scale as usize,
        };
        d.normalize();
        Some(d)
    }

    /// Strip redundant leading zeros and canonicalize the sign of zero. Trailing
    /// (fractional) zeros are *kept* — they are significant for `numeric`'s
    /// displayed scale, matching PostgreSQL (`1.50` stays `1.50`).
    fn normalize(&mut self) {
        // Ensure at least one digit.
        if self.digits.is_empty() {
            self.digits.push(0);
        }
        // Count of integer-part digits.
        let int_len = self.digits.len().saturating_sub(self.scale);
        // Remove leading zeros from the integer part, but keep at least one digit
        // before the decimal point.
        let mut lead = 0;
        while lead + 1 < int_len && self.digits[lead] == 0 {
            lead += 1;
        }
        if lead > 0 {
            self.digits.drain(0..lead);
        }
        // Canonical zero: magnitude all zeros => positive.
        if self.digits.iter().all(|&d| d == 0) {
            self.negative = false;
        }
    }

    /// True if this value is exactly zero.
    pub fn is_zero(&self) -> bool {
        self.digits.iter().all(|&d| d == 0)
    }

    /// Negate (zero stays positive).
    pub fn neg(&self) -> BigDecimal {
        if self.is_zero() {
            return self.clone();
        }
        BigDecimal {
            negative: !self.negative,
            digits: self.digits.clone(),
            scale: self.scale,
        }
    }

    /// Return the unscaled magnitude as least-significant-first digits, padded so
    /// the fractional part has exactly `target_scale` digits.
    fn aligned_lsf(&self, target_scale: usize) -> Vec<u8> {
        let mut lsf: Vec<u8> = self.digits.iter().rev().copied().collect();
        // Prepend (in LSF terms, push at front) zeros to reach target_scale frac digits.
        if target_scale > self.scale {
            let pad = target_scale - self.scale;
            let mut padded = vec![0u8; pad];
            padded.extend_from_slice(&lsf);
            lsf = padded;
        }
        lsf
    }

    /// Add two non-negative LSF magnitudes.
    fn mag_add(a: &[u8], b: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
        let mut carry = 0u8;
        for i in 0..a.len().max(b.len()) {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            let s = x + y + carry;
            out.push(s % 10);
            carry = s / 10;
        }
        if carry > 0 {
            out.push(carry);
        }
        out
    }

    /// Subtract LSF magnitude `b` from `a`, requiring `a >= b`. Returns LSF result.
    fn mag_sub(a: &[u8], b: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(a.len());
        let mut borrow = 0i16;
        for (i, &ai) in a.iter().enumerate() {
            let x = ai as i16;
            let y = b.get(i).copied().unwrap_or(0) as i16;
            let mut diff = x - y - borrow;
            if diff < 0 {
                diff += 10;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out.push(diff as u8);
        }
        out
    }

    /// Compare two LSF magnitudes (ignoring trailing high-order zeros).
    fn mag_cmp(a: &[u8], b: &[u8]) -> Ordering {
        // Effective length ignoring leading zeros (which are at the high end / back).
        let la = {
            let mut l = a.len();
            while l > 0 && a[l - 1] == 0 {
                l -= 1;
            }
            l
        };
        let lb = {
            let mut l = b.len();
            while l > 0 && b[l - 1] == 0 {
                l -= 1;
            }
            l
        };
        if la != lb {
            return la.cmp(&lb);
        }
        for i in (0..la).rev() {
            match a[i].cmp(&b[i]) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }

    /// Build a normalized `BigDecimal` from an LSF magnitude and scale/sign.
    fn from_lsf(negative: bool, lsf: &[u8], scale: usize) -> BigDecimal {
        let digits: Vec<u8> = lsf.iter().rev().copied().collect();
        let mut d = BigDecimal {
            negative,
            digits,
            scale,
        };
        d.normalize();
        d
    }

    /// Exact addition.
    pub fn add(&self, other: &BigDecimal) -> BigDecimal {
        let scale = self.scale.max(other.scale);
        let a = self.aligned_lsf(scale);
        let b = other.aligned_lsf(scale);
        if self.negative == other.negative {
            let mag = BigDecimal::mag_add(&a, &b);
            BigDecimal::from_lsf(self.negative, &mag, scale)
        } else {
            // Differing signs: subtract the smaller magnitude from the larger.
            match BigDecimal::mag_cmp(&a, &b) {
                // Result is zero, but keep the common scale (PG: 1.5-1.5 = 0.0).
                Ordering::Equal => BigDecimal::from_lsf(false, &vec![0u8; scale + 1], scale),
                Ordering::Greater => {
                    let mag = BigDecimal::mag_sub(&a, &b);
                    BigDecimal::from_lsf(self.negative, &mag, scale)
                }
                Ordering::Less => {
                    let mag = BigDecimal::mag_sub(&b, &a);
                    BigDecimal::from_lsf(other.negative, &mag, scale)
                }
            }
        }
    }

    /// Exact subtraction.
    pub fn sub(&self, other: &BigDecimal) -> BigDecimal {
        self.add(&other.neg())
    }

    /// Exact multiplication. Result scale is the sum of operand scales (matching
    /// PostgreSQL, where `1.1 * 1.1 = 1.21`).
    pub fn mul(&self, other: &BigDecimal) -> BigDecimal {
        if self.is_zero() || other.is_zero() {
            return BigDecimal::zero();
        }
        let a: Vec<u8> = self.digits.iter().rev().copied().collect();
        let b: Vec<u8> = other.digits.iter().rev().copied().collect();
        let mut prod = vec![0u16; a.len() + b.len()];
        for (i, &x) in a.iter().enumerate() {
            let mut carry = 0u16;
            for (j, &y) in b.iter().enumerate() {
                let cur = prod[i + j] + (x as u16) * (y as u16) + carry;
                prod[i + j] = cur % 10;
                carry = cur / 10;
            }
            let mut k = i + b.len();
            while carry > 0 {
                let cur = prod[k] + carry;
                prod[k] = cur % 10;
                carry = cur / 10;
                k += 1;
            }
        }
        let lsf: Vec<u8> = prod.iter().map(|&d| d as u8).collect();
        BigDecimal::from_lsf(self.negative != other.negative, &lsf, self.scale + other.scale)
    }

    /// Exact-magnitude division rounded half-up to `DIV_SCALE` fractional digits.
    ///
    /// Returns `None` on division by zero (the caller raises the SQL error).
    pub fn div(&self, other: &BigDecimal) -> Option<BigDecimal> {
        self.div_scale(other, DIV_SCALE)
    }

    /// Division to an explicit target scale (rounded half-up).
    pub fn div_scale(&self, other: &BigDecimal, target_scale: usize) -> Option<BigDecimal> {
        if other.is_zero() {
            return None;
        }
        if self.is_zero() {
            // 0 / x => 0 at the requested scale.
            let mut lsf = vec![0u8; target_scale.max(1)];
            if lsf.is_empty() {
                lsf.push(0);
            }
            return Some(BigDecimal::from_lsf(false, &lsf, target_scale));
        }
        // Work with integer magnitudes. dividend_unscaled / 10^(self.scale),
        // divisor_unscaled / 10^(other.scale). We want quotient with
        // `target_scale` frac digits, plus one guard digit for rounding.
        //
        // quotient_digits = round( dividend * 10^(target_scale+1 + other.scale - self.scale) / divisor )
        // then round the guard digit half-up.
        let dividend: Vec<u8> = self.digits.clone(); // MSF
        let divisor: Vec<u8> = other.digits.clone(); // MSF

        let guard = target_scale + 1;
        // Net power-of-ten to multiply the dividend by before integer division.
        let shift: i64 = guard as i64 + other.scale as i64 - self.scale as i64;

        // Build the scaled dividend as MSF digits.
        let mut num: Vec<u8> = dividend.clone();
        if shift >= 0 {
            num.resize(num.len() + shift as usize, 0);
        } else {
            // Negative shift: we would drop low-order digits. Since guard is
            // large this is rare, but handle it by truncating (then rounding via
            // the long-division remainder below is unaffected because we already
            // lost precision — pad instead to be safe). Pad divisor instead.
            // Simpler: never let shift go negative by padding num minimally.
            // (shift can only be negative when self.scale is very large.)
            let extra = (-shift) as usize;
            // Multiply divisor by 10^extra to achieve same ratio without losing
            // dividend digits.
            let mut d2 = divisor.clone();
            d2.resize(d2.len() + extra, 0);
            return Self::finish_div(self.negative != other.negative, &num, &d2, target_scale);
        }

        Self::finish_div(self.negative != other.negative, &num, &divisor, target_scale)
    }

    /// Long-division of MSF magnitudes `num / den`, producing a quotient that has
    /// one guard digit beyond `target_scale`, then rounding half-up to drop it.
    fn finish_div(negative: bool, num: &[u8], den: &[u8], target_scale: usize) -> Option<BigDecimal> {
        // Schoolbook long division over decimal digits (MSF).
        let mut quotient: Vec<u8> = Vec::with_capacity(num.len());
        // Remainder kept as MSF magnitude (no leading zeros, or [0]).
        let mut rem: Vec<u8> = vec![0];
        let den_lsf: Vec<u8> = den.iter().rev().copied().collect();
        for &d in num {
            // rem = rem * 10 + d
            rem.push(d);
            trim_leading_zeros(&mut rem);
            // Find largest q in 0..=9 with den*q <= rem.
            let rem_lsf: Vec<u8> = rem.iter().rev().copied().collect();
            let mut q = 0u8;
            let mut lo = 0u8;
            let mut hi = 9u8;
            while lo <= hi {
                let mid = (lo + hi) / 2;
                let prod = mag_scale(&den_lsf, mid); // LSF
                if BigDecimal::mag_cmp(&prod, &rem_lsf) != Ordering::Greater {
                    q = mid;
                    if mid == 9 {
                        break;
                    }
                    lo = mid + 1;
                } else {
                    if mid == 0 {
                        break;
                    }
                    hi = mid - 1;
                }
            }
            quotient.push(q);
            // rem = rem - den*q
            let prod = mag_scale(&den_lsf, q); // LSF
            let new_rem_lsf = BigDecimal::mag_sub(&rem_lsf, &prod);
            rem = new_rem_lsf.iter().rev().copied().collect();
            trim_leading_zeros(&mut rem);
        }
        // `quotient` has one guard digit at the end (because num was scaled by
        // guard = target_scale + 1). Round half-up using the guard digit, and any
        // nonzero remainder bumps a halfway case up as well.
        let guard_digit = *quotient.last().unwrap_or(&0);
        quotient.pop();
        // Half-up: round up whenever the dropped guard digit is >= 5.
        let round_up = guard_digit >= 5;
        let mut q_lsf: Vec<u8> = quotient.iter().rev().copied().collect();
        if q_lsf.is_empty() {
            q_lsf.push(0);
        }
        if round_up {
            q_lsf = BigDecimal::mag_add(&q_lsf, &[1]);
        }
        Some(BigDecimal::from_lsf(negative, &q_lsf, target_scale))
    }

    /// Exact remainder `self mod other`, with the sign of the dividend (PostgreSQL
    /// numeric `%`). Returns `None` on division by zero.
    pub fn rem(&self, other: &BigDecimal) -> Option<BigDecimal> {
        if other.is_zero() {
            return None;
        }
        // Align both to a common scale so we can divide integer magnitudes.
        let scale = self.scale.max(other.scale);
        let a_lsf = self.aligned_lsf(scale);
        let b_lsf = other.aligned_lsf(scale);
        let a_msf: Vec<u8> = a_lsf.iter().rev().copied().collect();
        let b_msf: Vec<u8> = b_lsf.iter().rev().copied().collect();
        // Truncated long division: remainder magnitude r with 0 <= r < |b|.
        let mut rem: Vec<u8> = vec![0];
        let den_lsf: Vec<u8> = b_msf.iter().rev().copied().collect();
        for &d in &a_msf {
            rem.push(d);
            trim_leading_zeros(&mut rem);
            let rem_lsf: Vec<u8> = rem.iter().rev().copied().collect();
            let mut q = 0u8;
            for cand in (0u8..=9).rev() {
                let prod = mag_scale(&den_lsf, cand);
                if BigDecimal::mag_cmp(&prod, &rem_lsf) != Ordering::Greater {
                    q = cand;
                    break;
                }
            }
            let prod = mag_scale(&den_lsf, q);
            let new_rem = BigDecimal::mag_sub(&rem_lsf, &prod);
            rem = new_rem.iter().rev().copied().collect();
            trim_leading_zeros(&mut rem);
        }
        // `rem` is the remainder magnitude at the aligned scale.
        let rem_lsf: Vec<u8> = rem.iter().rev().copied().collect();
        Some(BigDecimal::from_lsf(self.negative, &rem_lsf, scale))
    }

    /// Render in canonical PostgreSQL `numeric` text form, preserving scale
    /// (trailing zeros). No exponent.
    pub fn to_canonical_string(&self) -> String {
        let n = self.digits.len();
        let int_len = n.saturating_sub(self.scale);
        let mut out = String::new();
        if self.negative && !self.is_zero() {
            out.push('-');
        }
        if int_len == 0 {
            out.push('0');
        } else {
            for &d in &self.digits[0..int_len] {
                out.push((b'0' + d) as char);
            }
        }
        if self.scale > 0 {
            out.push('.');
            // The fractional part is the last `scale` digits. When there are
            // fewer stored digits than `scale` (e.g. `15` with scale 3 = 0.015),
            // left-pad the fractional part with zeros.
            let pad = self.scale.saturating_sub(n);
            for _ in 0..pad {
                out.push('0');
            }
            let frac_start = n.saturating_sub(self.scale);
            for &d in &self.digits[frac_start..] {
                out.push((b'0' + d) as char);
            }
        }
        out
    }

    /// Convert to `f64` (lossy), used for mixed numeric/float arithmetic.
    pub fn to_f64(&self) -> f64 {
        self.to_canonical_string().parse::<f64>().unwrap_or(f64::NAN)
    }
}

/// Trim leading (high-order, front) zeros from an MSF magnitude, keeping one digit.
fn trim_leading_zeros(msf: &mut Vec<u8>) {
    let mut lead = 0;
    while lead + 1 < msf.len() && msf[lead] == 0 {
        lead += 1;
    }
    if lead > 0 {
        msf.drain(0..lead);
    }
}

/// Multiply an LSF magnitude by a single decimal digit `q`, returning LSF.
fn mag_scale(lsf: &[u8], q: u8) -> Vec<u8> {
    if q == 0 {
        return vec![0];
    }
    let mut out = Vec::with_capacity(lsf.len() + 1);
    let mut carry = 0u16;
    for &d in lsf {
        let cur = (d as u16) * (q as u16) + carry;
        out.push((cur % 10) as u8);
        carry = cur / 10;
    }
    while carry > 0 {
        out.push((carry % 10) as u8);
        carry /= 10;
    }
    out
}

impl PartialEq for BigDecimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for BigDecimal {}

impl PartialOrd for BigDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigDecimal {
    fn cmp(&self, other: &Self) -> Ordering {
        // Zero is sign-neutral.
        let lz = self.is_zero();
        let rz = other.is_zero();
        if lz && rz {
            return Ordering::Equal;
        }
        match (self.negative, other.negative) {
            (false, true) => return Ordering::Greater,
            (true, false) => return Ordering::Less,
            _ => {}
        }
        // Same sign: compare magnitudes at a common scale.
        let scale = self.scale.max(other.scale);
        let a = self.aligned_lsf(scale);
        let b = other.aligned_lsf(scale);
        let ord = BigDecimal::mag_cmp(&a, &b);
        if self.negative {
            ord.reverse()
        } else {
            ord
        }
    }
}

impl fmt::Display for BigDecimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_canonical_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bd(s: &str) -> BigDecimal {
        BigDecimal::parse(s).unwrap()
    }

    #[test]
    fn parse_round_trip() {
        for s in [
            "0", "1", "-1", "123", "-123", "0.1", "0.10", "1.50", "-0.001",
            "1000000", "0.000001", "12345678901234567890.123456789",
        ] {
            assert_eq!(bd(s).to_canonical_string(), s);
        }
    }

    #[test]
    fn parse_exponent_and_signs() {
        assert_eq!(bd("1e3").to_canonical_string(), "1000");
        assert_eq!(bd("1.5e2").to_canonical_string(), "150");
        assert_eq!(bd("1.5e-2").to_canonical_string(), "0.015");
        assert_eq!(bd("+42").to_canonical_string(), "42");
        assert_eq!(bd(".5").to_canonical_string(), "0.5");
        assert_eq!(bd("-.25").to_canonical_string(), "-0.25");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(BigDecimal::parse("").is_none());
        assert!(BigDecimal::parse("abc").is_none());
        assert!(BigDecimal::parse("1.2.3").is_none());
        assert!(BigDecimal::parse("1e").is_none());
        assert!(BigDecimal::parse("1.2e3x").is_none());
    }

    #[test]
    fn classic_point_one_plus_point_two() {
        let r = bd("0.1").add(&bd("0.2"));
        assert_eq!(r.to_canonical_string(), "0.3");
    }

    #[test]
    fn add_scale_alignment() {
        assert_eq!(bd("1.5").add(&bd("2.25")).to_canonical_string(), "3.75");
        assert_eq!(bd("1.50").add(&bd("0.50")).to_canonical_string(), "2.00");
        assert_eq!(bd("100").add(&bd("0.001")).to_canonical_string(), "100.001");
    }

    #[test]
    fn add_carry_across_limbs() {
        assert_eq!(bd("999999999999999999").add(&bd("1")).to_canonical_string(), "1000000000000000000");
        assert_eq!(bd("9.99").add(&bd("0.01")).to_canonical_string(), "10.00");
    }

    #[test]
    fn subtract_and_borrow() {
        assert_eq!(bd("1000000000000000000").sub(&bd("1")).to_canonical_string(), "999999999999999999");
        assert_eq!(bd("0.3").sub(&bd("0.1")).to_canonical_string(), "0.2");
        assert_eq!(bd("5").sub(&bd("8")).to_canonical_string(), "-3");
        assert_eq!(bd("1.5").sub(&bd("1.5")).to_canonical_string(), "0.0");
    }

    #[test]
    fn negatives_mixed_signs() {
        assert_eq!(bd("-5").add(&bd("3")).to_canonical_string(), "-2");
        assert_eq!(bd("-5").add(&bd("8")).to_canonical_string(), "3");
        assert_eq!(bd("-5").sub(&bd("-8")).to_canonical_string(), "3");
        assert_eq!(bd("-0.1").add(&bd("-0.2")).to_canonical_string(), "-0.3");
    }

    #[test]
    fn multiply() {
        assert_eq!(bd("1.1").mul(&bd("1.1")).to_canonical_string(), "1.21");
        assert_eq!(bd("1.50").mul(&bd("2")).to_canonical_string(), "3.00");
        assert_eq!(bd("-3").mul(&bd("4")).to_canonical_string(), "-12");
        assert_eq!(bd("0").mul(&bd("123.45")).to_canonical_string(), "0");
    }

    #[test]
    fn multiply_very_large() {
        let big = "123456789012345678901234567890";
        assert_eq!(
            bd(big).mul(&BigDecimal::from_i64(2)).to_canonical_string(),
            "246913578024691357802469135780"
        );
        // (10^15)^2 exact.
        assert_eq!(
            bd("1000000000000000").mul(&bd("1000000000000000")).to_canonical_string(),
            "1000000000000000000000000000000"
        );
    }

    #[test]
    fn division_rounding() {
        // 1/3 rounded half-up to 16 frac digits.
        assert_eq!(bd("1").div(&bd("3")).unwrap().to_canonical_string(), "0.3333333333333333");
        // 2/3 -> ...6667 (round up).
        assert_eq!(bd("2").div(&bd("3")).unwrap().to_canonical_string(), "0.6666666666666667");
        // Exact-ish division still padded to target scale.
        assert_eq!(bd("1").div(&bd("4")).unwrap().to_canonical_string(), "0.2500000000000000");
        // 10/2.
        assert_eq!(bd("10").div(&bd("2")).unwrap().to_canonical_string(), "5.0000000000000000");
        // Negative.
        assert_eq!(bd("-1").div(&bd("3")).unwrap().to_canonical_string(), "-0.3333333333333333");
    }

    #[test]
    fn division_by_zero_is_none() {
        assert!(bd("1").div(&bd("0")).is_none());
    }

    #[test]
    fn div_explicit_scale_half_up() {
        // 1/8 = 0.125 -> 2 places rounds half-up to 0.13.
        assert_eq!(bd("1").div_scale(&bd("8"), 2).unwrap().to_canonical_string(), "0.13");
        // 1/8 at 3 places exact.
        assert_eq!(bd("1").div_scale(&bd("8"), 3).unwrap().to_canonical_string(), "0.125");
    }

    #[test]
    fn ordering() {
        assert!(bd("0.1") < bd("0.2"));
        assert!(bd("-1") < bd("0"));
        assert!(bd("-5") < bd("-4"));
        assert!(bd("1.50") == bd("1.5"));
        assert!(bd("100") > bd("99.999"));
        assert!(bd("0") == bd("-0"));
        let mut v = vec![bd("3"), bd("-1.5"), bd("2.25"), bd("0"), bd("-10")];
        v.sort();
        let got: Vec<String> = v.iter().map(|x| x.to_canonical_string()).collect();
        assert_eq!(got, vec!["-10", "-1.5", "0", "2.25", "3"]);
    }

    #[test]
    fn is_zero_and_neg() {
        assert!(bd("0").is_zero());
        assert!(bd("0.000").is_zero());
        assert!(!bd("0.001").is_zero());
        assert_eq!(bd("0").neg().to_canonical_string(), "0");
        assert_eq!(bd("5").neg().to_canonical_string(), "-5");
        assert_eq!(bd("-5").neg().to_canonical_string(), "5");
    }

    #[test]
    fn from_i64_edge() {
        assert_eq!(BigDecimal::from_i64(0).to_canonical_string(), "0");
        assert_eq!(BigDecimal::from_i64(i64::MAX).to_canonical_string(), "9223372036854775807");
        assert_eq!(BigDecimal::from_i64(i64::MIN).to_canonical_string(), "-9223372036854775808");
    }
}
