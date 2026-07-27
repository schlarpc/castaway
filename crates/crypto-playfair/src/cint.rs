//! C integer semantics, as a type.
//!
//! The FairPlay derivation is transcribed from C that leans on integer promotion,
//! wrap-around and truncation-on-store at nearly every line. Rather than rewrite each
//! expression into `wrapping_add`/`wrapping_mul` calls — where one missed call is a
//! panic in debug and a wrong key in release — the arithmetic gets a type that behaves
//! the way the original did, so the expressions can be carried across as written.
//!
//! [`W`] is a 32-bit unsigned value whose every operator wraps. That matches C's
//! `unsigned int`, which is what these expressions evaluate in: every operand is either
//! an `unsigned char` promoted to `int` or an `unsigned int` temporary, and C's usual
//! arithmetic conversions make the whole expression unsigned as soon as one temporary
//! is involved.
//!
//! The one place this could diverge is division: C would divide as *signed* in a
//! subexpression made only of promoted bytes and literals, and a negative dividend would
//! then round toward zero rather than becoming a huge unsigned value. Every divisor here
//! is a small positive literal and every dividend a sum or product of byte values, so
//! the two agree — and the twenty published test vectors are what actually settles it.

use std::ops::{Add, BitAnd, BitOr, BitXor, Div, Mul, Neg, Not, Rem, Shl, Shr, Sub};

/// A C `unsigned int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct W(pub u32);

impl W {
    /// Truncate to a byte, as C does when storing into an `unsigned char`.
    pub(crate) const fn u8(self) -> u8 {
        #[allow(clippy::cast_possible_truncation)]
        {
            self.0 as u8
        }
    }

    /// As an index. Callers have already reduced by a modulus.
    pub(crate) const fn idx(self) -> usize {
        self.0 as usize
    }
}

impl From<u8> for W {
    fn from(v: u8) -> Self {
        Self(u32::from(v))
    }
}

macro_rules! wrapping_binop {
    ($trait:ident, $method:ident, $wrapping:ident) => {
        impl $trait for W {
            type Output = Self;
            fn $method(self, rhs: Self) -> Self {
                Self(self.0.$wrapping(rhs.0))
            }
        }
    };
}

wrapping_binop!(Add, add, wrapping_add);
wrapping_binop!(Sub, sub, wrapping_sub);
wrapping_binop!(Mul, mul, wrapping_mul);
// Every divisor in the transcription is a positive literal, so this cannot trap.
wrapping_binop!(Div, div, wrapping_div);
wrapping_binop!(Rem, rem, wrapping_rem);
// C leaves a shift of 32 or more undefined; masking the count is what the hardware these
// tables were recovered on actually does.
wrapping_binop!(Shl, shl, wrapping_shl);
wrapping_binop!(Shr, shr, wrapping_shr);

macro_rules! bitop {
    ($trait:ident, $method:ident, $op:tt) => {
        impl $trait for W {
            type Output = Self;
            fn $method(self, rhs: Self) -> Self {
                Self(self.0 $op rhs.0)
            }
        }
    };
}

bitop!(BitAnd, bitand, &);
bitop!(BitOr, bitor, |);
bitop!(BitXor, bitxor, ^);

impl Neg for W {
    type Output = Self;
    fn neg(self) -> Self {
        Self(self.0.wrapping_neg())
    }
}

impl Not for W {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}

/// C's `rol8` from the transcribed sources: an 8-bit rotate that stores back as a byte.
///
/// Note `count == 0` is a genuine no-op here, because C's `x >> 8` on a promoted byte is
/// zero rather than the wrap a rotate would give — and the two happen to agree.
pub(crate) fn rol8(input: W, count: W) -> W {
    let x = input.u8();
    W::from(x.rotate_left(count.0 % 8))
}

/// `rol8x`: like [`rol8`] but the result is *not* truncated to a byte.
///
/// The high bits it leaves set are load-bearing — the result feeds a `% 21`, so
/// truncating would pick a different table entry.
pub(crate) fn rol8x(input: W, count: W) -> W {
    let x = u32::from(input.u8());
    if count.0 == 0 {
        // `x >> 8` is zero for a promoted byte, so the expression degenerates to `x`.
        return W(x);
    }
    W((x << count.0) | (x >> (8 - count.0)))
}

/// `weird_ror8`: a right rotate that returns **zero** for a count of zero.
///
/// That is not a rotate, and it is not a mistake in the transcription — the original
/// says so explicitly, and the degenerate case is reachable.
pub(crate) fn weird_ror8(input: W, count: W) -> W {
    if count.0 == 0 {
        return W(0);
    }
    let x = u32::from(input.u8());
    W(((x >> count.0) & 0xff) | ((x & 0xff) << (8 - count.0)))
}

/// `weird_rol8`: the mirror of [`weird_ror8`], zero for a count of zero.
pub(crate) fn weird_rol8(input: W, count: W) -> W {
    if count.0 == 0 {
        return W(0);
    }
    let x = u32::from(input.u8());
    W(((x << count.0) & 0xff) | ((x & 0xff) >> (8 - count.0)))
}

/// `weird_rol32`: the un-truncated cousin, also zero for a count of zero.
pub(crate) fn weird_rol32(input: W, count: W) -> W {
    if count.0 == 0 {
        return W(0);
    }
    let x = u32::from(input.u8());
    W((x << count.0) ^ (x >> (8 - count.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_wraps_rather_than_panicking() {
        // The whole point: in debug, plain `u32` arithmetic would panic here, and the
        // transcribed expressions overflow constantly.
        assert_eq!(W(0) - W(1), W(u32::MAX));
        assert_eq!(W(0xFFFF_FFFF) * W(3), W(0xFFFF_FFFD));
    }

    #[test]
    fn storing_truncates_to_a_byte() {
        assert_eq!((W(0x1234_5678)).u8(), 0x78);
    }

    #[test]
    fn rol8x_keeps_the_bits_above_a_byte() {
        // 0x81 rotated by 4 is 0x18 as a byte, but the untruncated value is 0x818,
        // and `% 21` gives a different answer for each.
        assert_eq!(rol8x(W(0x81), W(4)).0, 0x818);
        assert_eq!(rol8(W(0x81), W(4)).0, 0x18);
    }

    #[test]
    fn the_weird_rotates_are_zero_at_zero() {
        assert_eq!(weird_ror8(W(0xAB), W(0)), W(0));
        assert_eq!(weird_rol8(W(0xAB), W(0)), W(0));
        // …and elsewhere they rotate *without* truncating, like `rol8x`: the high bits
        // survive, and the callers feed the result to a `% 21`.
        assert_eq!(weird_ror8(W(0x81), W(4)).0, 0x818);
        assert_eq!(weird_rol8(W(0x81), W(4)).0, 0x18);
    }

    #[test]
    fn rol8_of_zero_is_the_identity() {
        assert_eq!(rol8(W(0xAB), W(0)), W(0xAB));
    }
}
