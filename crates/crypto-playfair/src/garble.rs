//! The `garble` step of the FairPlay v3 key derivation.
//!
//! Transcribed mechanically from the published implementation rather than by hand.
//! It is four hundred lines of straight-line bit manipulation whose own author
//! wrote that he did not know what it was doing, and a transcription slip here
//! would not fail loudly — it would yield a wrong key, and a mirroring session
//! that connects and shows static. The arithmetic runs in [`W`], which reproduces
//! C's wrapping `unsigned int` semantics, so expressions carry across as written.
//!
//! What settles correctness is the twenty published `(key message, ekey, expected
//! key)` vectors in `tests/vectors.rs`, not reading this.
// Parentheses are carried across from the C exactly as written: this file is
// generated, and "tidying" it by hand is how a transcription acquires a bug.
#![allow(
    unused_parens,
    clippy::precedence,
    clippy::many_single_char_names,
    clippy::similar_names
)]

use crate::cint::{rol8, rol8x, weird_rol32, weird_rol8, weird_ror8, W};

/// The five buffers `sap_hash` grinds together, mutated in place.
#[allow(clippy::too_many_lines, unused_assignments)]
pub(crate) fn garble(
    b0: &mut [u8; 20],
    b1: &mut [u8; 210],
    b2: &mut [u8; 35],
    b3: &mut [u8; 132],
    b4: &mut [u8; 21],
) {
    let (
        mut tmp,
        mut tmp2,
        mut tmp3,
        mut a,
        mut b,
        mut c,
        mut d,
        mut e,
        mut m,
        mut j,
        mut g,
        mut f,
        mut h,
        mut k,
        mut r,
        mut s,
        mut t,
        mut u,
        mut v,
        mut w_,
        mut x,
        mut y,
        mut z,
    ) = (
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
        W(0),
    );

    b2[(W(0xc)).idx()] = (W(0x14)
        + (((W::from(b1[(W(0x40)).idx()]) & W(0x5c))
            | ((W::from(b1[(W(0x63)).idx()]) / W(0x3)) & W(0x23)))
            & W::from(
                b4[(rol8x(
                    W::from(b4[(W::from(b1[(W(0xce)).idx()]) % W(0x15)).idx()]),
                    W(0x4),
                ) % W(0x15))
                .idx()],
            )))
    .u8();
    b1[(W(0x4)).idx()] = ((W::from(b1[(W(0x63)).idx()]) / W(0x5))
        * (W::from(b1[(W(0x63)).idx()]) / W(0x5))
        * W(0x2))
    .u8();
    b2[(W(0x22)).idx()] = (W(0xb8)).u8();
    b1[(W(0x99)).idx()] = (W::from(b1[(W(0x99)).idx()])
        ^ (W::from(b2[(W::from(b1[(W(0xcb)).idx()]) % W(0x23)).idx()])
            * W::from(b2[(W::from(b1[(W(0xcb)).idx()]) % W(0x23)).idx()])
            * W::from(b1[(W(0xbe)).idx()])))
    .u8();
    b0[(W(0x3)).idx()] = (W::from(b0[(W(0x3)).idx()])
        - (((W::from(b4[(W::from(b1[(W(0xcd)).idx()]) % W(0x15)).idx()]) >> W(0x1)) & W(0x50))
            | W(0xe6440)))
    .u8();
    b0[(W(0x10)).idx()] = (W(0x93)).u8();
    b0[(W(0xd)).idx()] = (W(0x62)).u8();
    b1[(W(0x21)).idx()] = (W::from(b1[(W(0x21)).idx()])
        - (W::from(b4[(W::from(b1[(W(0x24)).idx()]) % W(0x15)).idx()]) & W(0xf6)))
    .u8();
    tmp2 = W::from(b2[(W::from(b1[(W(0x43)).idx()]) % W(0x23)).idx()]);
    b2[(W(0xc)).idx()] = (W(0x7)).u8();
    tmp = W::from(b0[(W::from(b1[(W(0xb5)).idx()]) % W(0x14)).idx()]);
    b1[(W(0x2)).idx()] = (W::from(b1[(W(0x2)).idx()]) - (W(0xc40))).u8();
    b0[(W(0x13)).idx()] = (W::from(b4[(W::from(b1[(W(0x3a)).idx()]) % W(0x15)).idx()])).u8();
    b3[(W(0x0)).idx()] =
        (W(0x5c) - W::from(b2[(W::from(b1[(W(0x20)).idx()]) % W(0x23)).idx()])).u8();
    b3[(W(0x4)).idx()] =
        (W::from(b2[(W::from(b1[(W(0xf)).idx()]) % W(0x23)).idx()]) + W(0x9e)).u8();
    b1[(W(0x22)).idx()] = (W::from(b1[(W(0x22)).idx()])
        + (W::from(
            b4[(((W::from(b2[(W::from(b1[(W(0xf)).idx()]) % W(0x23)).idx()]) + W(0x9e))
                & W(0xff))
                % W(0x15))
            .idx()],
        ) / W(0x5)))
    .u8();
    b0[(W(0x13)).idx()] = (W::from(b0[(W(0x13)).idx()])
        + (W(0xfffffee6)
            - ((W::from(b0[(W::from(b3[(W(0x4)).idx()]) % W(0x14)).idx()]) >> W(0x1)) & W(0x66))))
    .u8();
    b1[(W(0xf)).idx()] = ((W(0x3)
        * (((W::from(b1[(W(0x48)).idx()])
            >> (W::from(b4[(W::from(b1[(W(0xbe)).idx()]) % W(0x15)).idx()]) & W(0x7)))
            ^ (W::from(b1[(W(0x48)).idx()])
                << (W(0x7)
                    - (W::from(b4[(W::from(b1[(W(0xbe)).idx()]) % W(0x15)).idx()]) - W(0x1))
                    & W(0x7))))
            - (W(0x3) * W::from(b4[(W::from(b1[(W(0x7e)).idx()]) % W(0x15)).idx()]))))
        ^ W::from(b1[(W(0xf)).idx()]))
    .u8();
    b0[(W(0xf)).idx()] = (W::from(b0[(W(0xf)).idx()])
        ^ (W::from(b2[(W::from(b1[(W(0xb5)).idx()]) % W(0x23)).idx()])
            * W::from(b2[(W::from(b1[(W(0xb5)).idx()]) % W(0x23)).idx()])
            * W::from(b2[(W::from(b1[(W(0xb5)).idx()]) % W(0x23)).idx()])))
    .u8();
    b2[(W(0x4)).idx()] =
        (W::from(b2[(W(0x4)).idx()]) ^ (W::from(b1[(W(0xca)).idx()]) / W(0x3))).u8();
    a = W(0x5c) - W::from(b0[(W::from(b3[(W(0x0)).idx()]) % W(0x14)).idx()]);
    e = (a & W(0xc6))
        | (!W::from(b1[(W(0x69)).idx()]) & W(0xc6))
        | (a & (!W::from(b1[(W(0x69)).idx()])));
    b2[(W(0x1)).idx()] = (W::from(b2[(W(0x1)).idx()]) + (e * e * e)).u8();
    b0[(W(0x13)).idx()] = (W::from(b0[(W(0x13)).idx()])
        ^ (((W(0xe0) | (W::from(b4[(W::from(b1[(W(0x5c)).idx()]) % W(0x15)).idx()]) & W(0x1b)))
            * W::from(b2[(W::from(b1[(W(0x29)).idx()]) % W(0x23)).idx()]))
            / W(0x3)))
    .u8();
    b1[(W(0x8c)).idx()] = (W::from(b1[(W(0x8c)).idx()])
        + (weird_ror8(W(0x5c), W::from(b1[(W(0x5)).idx()]) & W(0x7))))
    .u8();
    b2[(W(0xc)).idx()] = (W::from(b2[(W(0xc)).idx()])
        + (((((!W::from(b1[(W(0x4)).idx()]))
            ^ W::from(b2[(W::from(b1[(W(0xc)).idx()]) % W(0x23)).idx()]))
            | W::from(b1[(W(0xb6)).idx()]))
            & W(0xc0))
            | (((!W::from(b1[(W(0x4)).idx()]))
                ^ W::from(b2[(W::from(b1[(W(0xc)).idx()]) % W(0x23)).idx()]))
                & W::from(b1[(W(0xb6)).idx()]))))
    .u8();
    b1[(W(0x24)).idx()] = (W::from(b1[(W(0x24)).idx()]) + (W(0x7d))).u8();
    b1[(W(0x7c)).idx()] = (rol8x(
        (((W(0x4a) & W::from(b1[(W(0x8a)).idx()]))
            | ((W(0x4a) | W::from(b1[(W(0x8a)).idx()])) & W::from(b0[(W(0xf)).idx()])))
            & W::from(b0[(W::from(b1[(W(0x2b)).idx()]) % W(0x14)).idx()]))
            | (((W(0x4a) & W::from(b1[(W(0x8a)).idx()]))
                | ((W(0x4a) | W::from(b1[(W(0x8a)).idx()])) & W::from(b0[(W(0xf)).idx()]))
                | W::from(b0[(W::from(b1[(W(0x2b)).idx()]) % W(0x14)).idx()]))
                & W(0x5f)),
        W(0x4),
    ))
    .u8();
    b3[(W(0x8)).idx()] = ((((W::from(b0[(W::from(b3[(W(0x4)).idx()]) % W(0x14)).idx()])
        & W(0x5f))
        & ((W::from(b4[(W::from(b1[(W(0x44)).idx()]) % W(0x15)).idx()]) & W(0x2e)) << W(0x1)))
        | W(0x10))
        ^ W(0x5c))
    .u8();
    a = W::from(b1[(W(0xb1)).idx()]) + W::from(b4[(W::from(b1[(W(0x4f)).idx()]) % W(0x15)).idx()]);
    d = (((a >> W(0x1)) | ((W(0x3) * W::from(b1[(W(0x94)).idx()])) / W(0x5)))
        & W::from(b2[(W(0x1)).idx()]))
        | ((a >> W(0x1)) & ((W(0x3) * W::from(b1[(W(0x94)).idx()])) / W(0x5)));
    b3[(W(0xc)).idx()] = (-W(0x22) - d).u8();
    a = W(0x8) - (W::from(b2[(W(0x16)).idx()]) & W(0x7));
    b = (W::from(b1[(W(0x21)).idx()]) >> (a & W(0x7)));
    c = W::from(b1[(W(0x21)).idx()]) << (W::from(b2[(W(0x16)).idx()]) & W(0x7));
    b2[(W(0x10)).idx()] = (W::from(b2[(W(0x10)).idx()])
        + (((W::from(b2[(W::from(b3[(W(0x0)).idx()]) % W(0x23)).idx()]) & W(0x9f))
            | W::from(b0[(W::from(b3[(W(0x4)).idx()]) % W(0x14)).idx()])
            | W(0x8))
            - ((b ^ c) | W(0x80))))
    .u8();
    b0[(W(0xe)).idx()] = (W::from(b0[(W(0xe)).idx()])
        ^ (W::from(b2[(W::from(b3[(W(0xc)).idx()]) % W(0x23)).idx()])))
    .u8();
    a = weird_rol8(
        W::from(b4[(W::from(b0[(W::from(b1[(W(0xc9)).idx()]) % W(0x14)).idx()]) % W(0x15)).idx()]),
        ((W::from(b2[(W::from(b1[(W(0x70)).idx()]) % W(0x23)).idx()]) << W(0x1)) & W(0x7)),
    );
    d = (W::from(b0[(W::from(b1[(W(0xd0)).idx()]) % W(0x14)).idx()]) & W(0x83))
        | (W::from(b0[(W::from(b1[(W(0xa4)).idx()]) % W(0x14)).idx()]) & W(0x7c));
    b1[(W(0x13)).idx()] =
        (W::from(b1[(W(0x13)).idx()]) + ((a & (d / W(0x5))) | ((a | (d / W(0x5))) & W(0x25)))).u8();
    b2[(W(0x8)).idx()] = (weird_ror8(
        W(0x8c),
        ((W::from(b4[(W::from(b1[(W(0x2d)).idx()]) % W(0x15)).idx()]) + W(0x5c))
            * (W::from(b4[(W::from(b1[(W(0x2d)).idx()]) % W(0x15)).idx()]) + W(0x5c)))
            & W(0x7),
    ))
    .u8();
    b1[(W(0xbe)).idx()] = (W(0x38)).u8();
    b2[(W(0x8)).idx()] = (W::from(b2[(W(0x8)).idx()]) ^ (W::from(b3[(W(0x0)).idx()]))).u8();
    b1[(W(0x35)).idx()] =
        (!((W::from(b0[(W::from(b1[(W(0x53)).idx()]) % W(0x14)).idx()]) | W(0xcc)) / W(0x5))).u8();
    b0[(W(0xd)).idx()] = (W::from(b0[(W(0xd)).idx()])
        + (W::from(b0[(W::from(b1[(W(0x29)).idx()]) % W(0x14)).idx()])))
    .u8();
    b0[(W(0xa)).idx()] = (((W::from(b2[(W::from(b3[(W(0x0)).idx()]) % W(0x23)).idx()])
        & W::from(b1[(W(0x2)).idx()]))
        | ((W::from(b2[(W::from(b3[(W(0x0)).idx()]) % W(0x23)).idx()])
            | W::from(b1[(W(0x2)).idx()]))
            & W::from(b3[(W(0xc)).idx()])))
        / W(0xf))
    .u8();
    a = (((W(0x38) | (W::from(b4[(W::from(b1[(W(0x2)).idx()]) % W(0x15)).idx()]) & W(0x44)))
        | W::from(b2[(W::from(b3[(W(0x8)).idx()]) % W(0x23)).idx()]))
        & W(0x2a))
        | (((W::from(b4[(W::from(b1[(W(0x2)).idx()]) % W(0x15)).idx()]) & W(0x44)) | W(0x38))
            & W::from(b2[(W::from(b3[(W(0x8)).idx()]) % W(0x23)).idx()]));
    b3[(W(0x10)).idx()] = ((a * a) + W(0x6e)).u8();
    b3[(W(0x14)).idx()] = (W(0xca) - W::from(b3[(W(0x10)).idx()])).u8();
    b3[(W(0x18)).idx()] = (W::from(b1[(W(0x97)).idx()])).u8();
    b2[(W(0xd)).idx()] = (W::from(b2[(W(0xd)).idx()])
        ^ (W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x15)).idx()])))
    .u8();
    b = ((W::from(b2[(W::from(b1[(W(0xb3)).idx()]) % W(0x23)).idx()]) - W(0x26)) & W(0xb1))
        | (W::from(b3[(W(0xc)).idx()]) & W(0xb1));
    c = (W::from(b2[(W::from(b1[(W(0xb3)).idx()]) % W(0x23)).idx()]) - W(0x26))
        & W::from(b3[(W(0xc)).idx()]);
    b3[(W(0x1c)).idx()] = (W(0x1e) + ((b | c) * (b | c))).u8();
    b3[(W(0x20)).idx()] = (W::from(b3[(W(0x1c)).idx()]) + W(0x3e)).u8();
    a = ((W::from(b3[(W(0x14)).idx()]) + (W::from(b3[(W(0x0)).idx()]) & W(0x4a)))
        | !W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x15)).idx()]))
        & W(0x79);
    b = ((W::from(b3[(W(0x14)).idx()]) + (W::from(b3[(W(0x0)).idx()]) & W(0x4a)))
        & !W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x15)).idx()]));
    tmp3 = (a | b);
    c = ((((a | b) ^ W(0xffffffa6)) | W::from(b3[(W(0x0)).idx()])) & W(0x4))
        | (((a | b) ^ W(0xffffffa6)) & W::from(b3[(W(0x0)).idx()]));
    b1[(W(0x2f)).idx()] = ((W::from(b2[(W::from(b1[(W(0x59)).idx()]) % W(0x23)).idx()]) + c)
        ^ W::from(b1[(W(0x2f)).idx()]))
    .u8();
    b3[(W(0x24)).idx()] = (((rol8((tmp & W(0xb3)) + W(0x44), W(0x2))
        & W::from(b0[(W(0x3)).idx()]))
        | (tmp2 & !W::from(b0[(W(0x3)).idx()])))
        - W(0xf))
    .u8();
    b1[(W(0x7b)).idx()] = (W::from(b1[(W(0x7b)).idx()]) ^ (W(0xdd))).u8();
    a = ((W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x15)).idx()])) / W(0x3))
        - W::from(b2[(W::from(b3[(W(0x4)).idx()]) % W(0x23)).idx()]);
    c = (((W::from(b3[(W(0x0)).idx()]) & W(0xa3)) + W(0x5c)) & W(0xf6))
        | (W::from(b3[(W(0x0)).idx()]) & W(0x5c));
    e = ((c | W::from(b3[(W(0x18)).idx()])) & W(0x36)) | (c & W::from(b3[(W(0x18)).idx()]));
    b3[(W(0x28)).idx()] = (a - e).u8();
    b3[(W(0x2c)).idx()] =
        (tmp3 ^ W(0x51) ^ (((W::from(b3[(W(0x0)).idx()]) >> W(0x1)) & W(0x65)) + W(0x1a))).u8();
    b3[(W(0x30)).idx()] =
        (W::from(b2[(W::from(b3[(W(0x4)).idx()]) % W(0x23)).idx()]) & W(0x1b)).u8();
    b3[(W(0x34)).idx()] = (W(0x1b)).u8();
    b3[(W(0x38)).idx()] = (W(0xc7)).u8();
    b3[(W(0x40)).idx()] = (W::from(b3[(W(0x4)).idx()])
        + (((((((W::from(b3[(W(0x28)).idx()]) | W::from(b3[(W(0x18)).idx()])) & W(0xb1))
            | (W::from(b3[(W(0x28)).idx()]) & W::from(b3[(W(0x18)).idx()])))
            & (((W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x14)).idx()]) & W(0xb1))
                | W(0xb0))
                | ((W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x15)).idx()])) & !W(0x3))))
            | ((((W::from(b3[(W(0x28)).idx()]) & W::from(b3[(W(0x18)).idx()]))
                | ((W::from(b3[(W(0x28)).idx()]) | W::from(b3[(W(0x18)).idx()])) & W(0xb1)))
                & W(0xc7))
                | ((((W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x15)).idx()]) & W(0x1))
                    + W(0xb0))
                    | (W::from(b4[(W::from(b3[(W(0x0)).idx()]) % W(0x15)).idx()]) & !W(0x3)))
                    & W::from(b3[(W(0x38)).idx()]))))
            & (!W::from(b3[(W(0x34)).idx()])))
            | W::from(b3[(W(0x30)).idx()])))
    .u8();
    b2[(W(0x21)).idx()] = (W::from(b2[(W(0x21)).idx()]) ^ (W::from(b1[(W(0x1a)).idx()]))).u8();
    b1[(W(0x6a)).idx()] =
        (W::from(b1[(W(0x6a)).idx()]) ^ (W::from(b3[(W(0x14)).idx()]) ^ W(0x85))).u8();
    b2[(W(0x1e)).idx()] = (((W::from(b3[(W(0x40)).idx()]) / W(0x3))
        - (W(0x113) | (W::from(b3[(W(0x0)).idx()]) & W(0xf7))))
        ^ W::from(b0[(W::from(b1[(W(0x7a)).idx()]) % W(0x14)).idx()]))
    .u8();
    b1[(W(0x16)).idx()] =
        ((W::from(b2[(W::from(b1[(W(0x5a)).idx()]) % W(0x23)).idx()]) & W(0x5f)) | W(0x44)).u8();
    a = (W::from(b4[(W::from(b3[(W(0x24)).idx()]) % W(0x15)).idx()]) & W(0xb8))
        | (W::from(b2[(W::from(b3[(W(0x2c)).idx()]) % W(0x23)).idx()]) & !W(0xb8));
    b2[(W(0x12)).idx()] = (W::from(b2[(W(0x12)).idx()]) + ((a * a * a) >> W(0x1))).u8();
    b2[(W(0x5)).idx()] = (W::from(b2[(W(0x5)).idx()])
        - (W::from(b4[(W::from(b1[(W(0x5c)).idx()]) % W(0x15)).idx()])))
    .u8();
    a = (((W::from(b1[(W(0x29)).idx()]) & !W(0x18))
        | (W::from(b2[(W::from(b1[(W(0xb7)).idx()]) % W(0x23)).idx()]) & W(0x18)))
        & (W::from(b3[(W(0x10)).idx()]) + W(0x35)))
        | (W::from(b3[(W(0x14)).idx()])
            & W::from(b2[(W::from(b3[(W(0x14)).idx()]) % W(0x23)).idx()]));
    b = (W::from(b1[(W(0x11)).idx()]) & (!W::from(b3[(W(0x2c)).idx()])))
        | (W::from(b0[(W::from(b1[(W(0x3b)).idx()]) % W(0x14)).idx()])
            & W::from(b3[(W(0x2c)).idx()]));
    b2[(W(0x12)).idx()] = (W::from(b2[(W(0x12)).idx()]) ^ (a * b)).u8();
    a = weird_ror8(
        W::from(b1[(W(0xb)).idx()]),
        W::from(b2[(W::from(b1[(W(0x1c)).idx()]) % W(0x23)).idx()]) & W(0x7),
    ) & W(0x7);
    b = (((W::from(b0[(W::from(b1[(W(0x5d)).idx()]) % W(0x14)).idx()])
        & !W::from(b0[(W(0xe)).idx()]))
        | (W::from(b0[(W(0xe)).idx()]) & W(0x96)))
        & !W(0x1c))
        | (W::from(b1[(W(0x7)).idx()]) & W(0x1c));
    b2[(W(0x16)).idx()] = (((((b | weird_rol8(
        W::from(b2[(W::from(b3[(W(0x0)).idx()]) % W(0x23)).idx()]),
        a,
    )) & W::from(b2[(W(0x21)).idx()]))
        | (b & weird_rol8(
            W::from(b2[(W::from(b3[(W(0x0)).idx()]) % W(0x23)).idx()]),
            a,
        )))
        + W(0x4a))
        & W(0xff))
    .u8();
    a = W::from(
        b4[((W::from(b0[(W::from(b1[(W(0x27)).idx()]) % W(0x14)).idx()]) ^ W(0xd9)) % W(0x15))
            .idx()],
    );
    b0[(W(0xf)).idx()] = (W::from(b0[(W(0xf)).idx()])
        - (((((W::from(b3[(W(0x14)).idx()]) | W::from(b3[(W(0x0)).idx()])) & W(0xd6))
            | (W::from(b3[(W(0x14)).idx()]) & W::from(b3[(W(0x0)).idx()])))
            & a)
            | ((((W::from(b3[(W(0x14)).idx()]) | W::from(b3[(W(0x0)).idx()])) & W(0xd6))
                | (W::from(b3[(W(0x14)).idx()]) & W::from(b3[(W(0x0)).idx()]))
                | a)
                & W::from(b3[(W(0x20)).idx()]))))
    .u8();
    b = (((W::from(b2[(W::from(b1[(W(0x39)).idx()]) % W(0x23)).idx()])
        & W::from(b0[(W::from(b3[(W(0x40)).idx()]) % W(0x14)).idx()]))
        | ((W::from(b0[(W::from(b3[(W(0x40)).idx()]) % W(0x14)).idx()])
            | W::from(b2[(W::from(b1[(W(0x39)).idx()]) % W(0x23)).idx()]))
            & W(0x5f))
        | (W::from(b3[(W(0x40)).idx()]) & W(0x2d))
        | W(0x52))
        & W(0x20));
    c = ((W::from(b2[(W::from(b1[(W(0x39)).idx()]) % W(0x23)).idx()])
        & W::from(b0[(W::from(b3[(W(0x40)).idx()]) % W(0x14)).idx()]))
        | ((W::from(b2[(W::from(b1[(W(0x39)).idx()]) % W(0x23)).idx()])
            | W::from(b0[(W::from(b3[(W(0x40)).idx()]) % W(0x14)).idx()]))
            & W(0x5f)))
        & ((W::from(b3[(W(0x40)).idx()]) & W(0x2d)) | W(0x52));
    d = (((W::from(b3[(W(0x0)).idx()]) / W(0x3))
        - (W::from(b3[(W(0x40)).idx()]) | W::from(b1[(W(0x16)).idx()])))
        ^ (W::from(b3[(W(0x1c)).idx()]) + W(0x3e))
        ^ (b | c));
    t = W::from(b0[((d & W(0xff)) % W(0x14)).idx()]);
    b3[(W(0x44)).idx()] = ((W::from(b0[(W::from(b1[(W(0x63)).idx()]) % W(0x14)).idx()])
        * W::from(b0[(W::from(b1[(W(0x63)).idx()]) % W(0x14)).idx()])
        * W::from(b0[(W::from(b1[(W(0x63)).idx()]) % W(0x14)).idx()])
        * W::from(b0[(W::from(b1[(W(0x63)).idx()]) % W(0x14)).idx()]))
        | W::from(b2[(W::from(b3[(W(0x40)).idx()]) % W(0x23)).idx()]))
    .u8();
    u = W::from(b0[(W::from(b1[(W(0x32)).idx()]) % W(0x14)).idx()]);
    w_ = W::from(b2[(W::from(b1[(W(0x8a)).idx()]) % W(0x23)).idx()]);
    x = W::from(b4[(W::from(b1[(W(0x27)).idx()]) % W(0x15)).idx()]);
    y = W::from(b0[(W::from(b1[(W(0x4)).idx()]) % W(0x14)).idx()]);
    z = W::from(b4[(W::from(b1[(W(0xca)).idx()]) % W(0x15)).idx()]);
    v = W::from(b0[(W::from(b1[(W(0x97)).idx()]) % W(0x14)).idx()]);
    s = W::from(b2[(W::from(b1[(W(0xe)).idx()]) % W(0x23)).idx()]);
    r = W::from(b0[(W::from(b1[(W(0x91)).idx()]) % W(0x14)).idx()]);
    a = (W::from(b2[(W::from(b3[(W(0x44)).idx()]) % W(0x23)).idx()])
        & W::from(b0[(W::from(b1[(W(0xd1)).idx()]) % W(0x14)).idx()]))
        | ((W::from(b2[(W::from(b3[(W(0x44)).idx()]) % W(0x23)).idx()])
            | W::from(b0[(W::from(b1[(W(0xd1)).idx()]) % W(0x14)).idx()]))
            & W(0x18));
    b = weird_rol8(
        W::from(b4[(W::from(b1[(W(0x7f)).idx()]) % W(0x15)).idx()]),
        W::from(b2[(W::from(b3[(W(0x44)).idx()]) % W(0x23)).idx()]) & W(0x7),
    );
    c = (a & W::from(b0[(W(0xa)).idx()])) | (b & !W::from(b0[(W(0xa)).idx()]));
    d = W(0x7)
        ^ (W::from(
            b4[(W::from(b2[(W::from(b3[(W(0x24)).idx()]) % W(0x23)).idx()]) % W(0x15)).idx()],
        ) << W(0x1));
    b3[(W(0x48)).idx()] = ((c & W(0x47)) | (d & !W(0x47))).u8();
    b2[(W(0x2)).idx()] = (W::from(b2[(W(0x2)).idx()])
        + ((((W::from(b0[(W::from(b3[(W(0x14)).idx()]) % W(0x14)).idx()]) << W(0x1)) & W(0x9f))
            | (W::from(b4[(W::from(b1[(W(0xbe)).idx()]) % W(0x15)).idx()]) & !W(0x9f)))
            & ((((W::from(b4[(W::from(b3[(W(0x40)).idx()]) % W(0x15)).idx()]) & W(0x6e))
                | (W::from(b0[(W::from(b1[(W(0x19)).idx()]) % W(0x14)).idx()]) & !W(0x6e)))
                & !W(0x96))
                | (W::from(b1[(W(0x19)).idx()]) & W(0x96)))))
    .u8();
    b2[(W(0xe)).idx()] = (W::from(b2[(W(0xe)).idx()])
        - (((W::from(b2[(W::from(b3[(W(0x14)).idx()]) % W(0x23)).idx()])
            & (W::from(b3[(W(0x48)).idx()])
                ^ W::from(b2[(W::from(b1[(W(0x64)).idx()]) % W(0x23)).idx()])))
            & !W(0x22))
            | (W::from(b1[(W(0x61)).idx()]) & W(0x22))))
    .u8();
    b0[(W(0x11)).idx()] = (W(0x73)).u8();
    b1[(W(0x17)).idx()] = (W::from(b1[(W(0x17)).idx()])
        ^ ((((((W::from(b4[(W::from(b1[(W(0x11)).idx()]) % W(0x15)).idx()])
            | W::from(b0[(W::from(b3[(W(0x14)).idx()]) % W(0x14)).idx()]))
            & W::from(b3[(W(0x48)).idx()]))
            | (W::from(b4[(W::from(b1[(W(0x11)).idx()]) % W(0x15)).idx()])
                & W::from(b0[(W::from(b3[(W(0x14)).idx()]) % W(0x14)).idx()])))
            & (W::from(b1[(W(0x32)).idx()]) / W(0x3)))
            | ((((W::from(b4[(W::from(b1[(W(0x11)).idx()]) % W(0x15)).idx()])
                | W::from(b0[(W::from(b3[(W(0x14)).idx()]) % W(0x14)).idx()]))
                & W::from(b3[(W(0x48)).idx()]))
                | (W::from(b4[(W::from(b1[(W(0x11)).idx()]) % W(0x15)).idx()])
                    & W::from(b0[(W::from(b3[(W(0x14)).idx()]) % W(0x14)).idx()]))
                | (W::from(b1[(W(0x32)).idx()]) / W(0x3)))
                & W(0xf6)))
            << W(0x1)))
    .u8();
    b0[(W(0xd)).idx()] = ((((((W::from(b0[(W::from(b3[(W(0x28)).idx()]) % W(0x14)).idx()])
        | W::from(b1[(W(0xa)).idx()]))
        & W(0x52))
        | (W::from(b0[(W::from(b3[(W(0x28)).idx()]) % W(0x14)).idx()])
            & W::from(b1[(W(0xa)).idx()])))
        & W(0xd1))
        | ((W::from(b0[(W::from(b1[(W(0x27)).idx()]) % W(0x14)).idx()]) << W(0x1)) & W(0x2e)))
        >> W(0x1))
    .u8();
    b2[(W(0x21)).idx()] =
        (W::from(b2[(W(0x21)).idx()]) - (W::from(b1[(W(0x71)).idx()]) & W(0x9))).u8();
    b2[(W(0x1c)).idx()] = (W::from(b2[(W(0x1c)).idx()])
        - ((((W(0x2) | (W::from(b1[(W(0x6e)).idx()]) & W(0xde))) >> W(0x1)) & !W(0xdf))
            | (W::from(b3[(W(0x14)).idx()]) & W(0xdf))))
    .u8();
    j = weird_rol8((v | z), (u & W(0x7)));
    a = (W::from(b2[(W(0x10)).idx()]) & t) | (w_ & (!W::from(b2[(W(0x10)).idx()])));
    b = (W::from(b1[(W(0x21)).idx()]) & W(0x11)) | (x & !W(0x11));
    e = ((y | ((a + b) / W(0x5))) & W(0x93)) | (y & ((a + b) / W(0x5)));
    m = (W::from(b3[(W(0x28)).idx()])
        & W::from(b4[(((W::from(b3[(W(0x8)).idx()]) + j + e) & W(0xff)) % W(0x15)).idx()]))
        | ((W::from(b3[(W(0x28)).idx()])
            | W::from(b4[(((W::from(b3[(W(0x8)).idx()]) + j + e) & W(0xff)) % W(0x15)).idx()]))
            & W::from(b2[(W(0x17)).idx()]));
    b0[(W(0xf)).idx()] = ((((W::from(b4[(W::from(b3[(W(0x14)).idx()]) % W(0x15)).idx()])
        - W(0x30))
        & (!W::from(b1[(W(0xb8)).idx()])))
        | ((W::from(b4[(W::from(b3[(W(0x14)).idx()]) % W(0x15)).idx()]) - W(0x30)) & W(0xbd))
        | (W(0xbd) & !W::from(b1[(W(0xb8)).idx()])))
        & (m * m * m))
        .u8();
    b2[(W(0x16)).idx()] = (W::from(b2[(W(0x16)).idx()]) + (W::from(b1[(W(0xb7)).idx()]))).u8();
    b3[(W(0x4c)).idx()] = ((W(0x3) * W::from(b4[(W::from(b1[(W(0x1)).idx()]) % W(0x15)).idx()]))
        ^ W::from(b3[(W(0x0)).idx()]))
    .u8();
    a = W::from(b2[(((W::from(b3[(W(0x8)).idx()]) + (j + e)) & W(0xff)) % W(0x23)).idx()]);
    f = (((W::from(b4[(W::from(b1[(W(0xb2)).idx()]) % W(0x15)).idx()]) & a)
        | ((W::from(b4[(W::from(b1[(W(0xb2)).idx()]) % W(0x15)).idx()]) | a) & W(0xd1)))
        * W::from(b0[(W::from(b1[(W(0xd)).idx()]) % W(0x14)).idx()]))
        * (W::from(b4[(W::from(b1[(W(0x1a)).idx()]) % W(0x15)).idx()]) >> W(0x1));
    g = (f + W(0x733ffff9)) * W(0xc6) - (((f + W(0x733ffff9)) * W(0x18c) + W(0xd4)) & W(0xd4))
        + W(0x55);
    b3[(W(0x50)).idx()] =
        (W::from(b3[(W(0x24)).idx()]) + (g ^ W(0x94)) + ((g ^ W(0x6b)) << W(0x1)) - W(0x7f)).u8();
    b3[(W(0x54)).idx()] = (((W::from(b2[(W::from(b3[(W(0x40)).idx()]) % W(0x23)).idx()]))
        & W(0xf5))
        | (W::from(b2[(W::from(b3[(W(0x14)).idx()]) % W(0x23)).idx()]) & W(0xa)))
    .u8();
    a = W::from(b0[(W::from(b3[(W(0x44)).idx()]) % W(0x14)).idx()]) | W(0x51);
    b2[(W(0x12)).idx()] = (W::from(b2[(W(0x12)).idx()])
        - (((a * a * a) & !W::from(b0[(W(0xf)).idx()]))
            | ((W::from(b3[(W(0x50)).idx()]) / W(0xf)) & W::from(b0[(W(0xf)).idx()]))))
    .u8();
    b3[(W(0x58)).idx()] = (W::from(b3[(W(0x8)).idx()]) + j + e
        - W::from(b0[(W::from(b1[(W(0xa0)).idx()]) % W(0x14)).idx()])
        + (W::from(
            b4[(W::from(b0[(((W::from(b3[(W(0x8)).idx()]) + j + e) & W(0xff)) % W(0x14)).idx()])
                % W(0x15))
            .idx()],
        ) / W(0x3)))
    .u8();
    b = ((r ^ W::from(b3[(W(0x48)).idx()])) & !W(0xc6)) | ((s * s) & W(0xc6));
    f = (W::from(b4[(W::from(b1[(W(0x45)).idx()]) % W(0x15)).idx()])
        & W::from(b1[(W(0xac)).idx()]))
        | ((W::from(b4[(W::from(b1[(W(0x45)).idx()]) % W(0x15)).idx()])
            | W::from(b1[(W(0xac)).idx()]))
            & ((W::from(b3[(W(0xc)).idx()]) - b) + W(0x4d)));
    b0[(W(0x10)).idx()] = (W(0x93)
        - ((W::from(b3[(W(0x48)).idx()]) & ((f & W(0xfb)) | W(0x1)))
            | (((f & W(0xfa)) | W::from(b3[(W(0x48)).idx()])) & W(0xc6))))
    .u8();
    c = (W::from(b4[(W::from(b1[(W(0xa8)).idx()]) % W(0x15)).idx()])
        & W::from(b0[(W::from(b1[(W(0x1d)).idx()]) % W(0x14)).idx()])
        & W(0x7))
        | ((W::from(b4[(W::from(b1[(W(0xa8)).idx()]) % W(0x15)).idx()])
            | W::from(b0[(W::from(b1[(W(0x1d)).idx()]) % W(0x14)).idx()]))
            & W(0x6));
    f = (W::from(b4[(W::from(b1[(W(0x9b)).idx()]) % W(0x15)).idx()])
        & W::from(b1[(W(0x69)).idx()]))
        | ((W::from(b4[(W::from(b1[(W(0x9b)).idx()]) % W(0x15)).idx()])
            | W::from(b1[(W(0x69)).idx()]))
            & W(0x8d));
    b0[(W(0x3)).idx()] =
        (W::from(b0[(W(0x3)).idx()]) - (W::from(b4[(weird_rol32(f, c) % W(0x15)).idx()]))).u8();
    b1[(W(0x5)).idx()] = (weird_ror8(
        W::from(b0[(W(0xc)).idx()]),
        ((W::from(b0[(W::from(b1[(W(0x3d)).idx()]) % W(0x14)).idx()]) / W(0x5)) & W(0x7)),
    ) ^ (((!W::from(b2[(W::from(b3[(W(0x54)).idx()]) % W(0x23)).idx()]))
        & W(0xffffffff))
        / W(0x5)))
    .u8();
    b1[(W(0xc6)).idx()] = (W::from(b1[(W(0xc6)).idx()]) + (W::from(b1[(W(0x3)).idx()]))).u8();
    a = (W(0xa2) | W::from(b2[(W::from(b3[(W(0x40)).idx()]) % W(0x23)).idx()]));
    b1[(W(0xa4)).idx()] = (W::from(b1[(W(0xa4)).idx()]) + ((a * a) / W(0x5))).u8();
    g = weird_ror8(W(0x8b), (W::from(b3[(W(0x50)).idx()]) & W(0x7)));
    c = ((W::from(b4[(W::from(b3[(W(0x40)).idx()]) % W(0x15)).idx()])
        * W::from(b4[(W::from(b3[(W(0x40)).idx()]) % W(0x15)).idx()])
        * W::from(b4[(W::from(b3[(W(0x40)).idx()]) % W(0x15)).idx()]))
        & W(0x5f))
        | (W::from(b0[(W::from(b3[(W(0x28)).idx()]) % W(0x14)).idx()]) & !W(0x5f));
    b3[(W(0x5c)).idx()] = ((g & W(0xc))
        | (W::from(b0[(W::from(b3[(W(0x14)).idx()]) % W(0x14)).idx()]) & W(0xc))
        | (g & W::from(b0[(W::from(b3[(W(0x14)).idx()]) % W(0x14)).idx()]))
        | c)
        .u8();
    b2[(W(0xc)).idx()] = (W::from(b2[(W(0xc)).idx()])
        + (((W::from(b1[(W(0x67)).idx()]) & W(0x20))
            | (W::from(b3[(W(0x5c)).idx()]) & (W::from(b1[(W(0x67)).idx()]) | W(0x3c)))
            | W(0x10))
            / W(0x3)))
    .u8();
    b3[(W(0x60)).idx()] = (W::from(b1[(W(0x8f)).idx()])).u8();
    b3[(W(0x64)).idx()] = (W(0x1b)).u8();
    b3[(W(0x68)).idx()] = ((((W::from(b3[(W(0x28)).idx()]) & !W::from(b2[(W(0x8)).idx()]))
        | (W::from(b1[(W(0x23)).idx()]) & W::from(b2[(W(0x8)).idx()])))
        & W::from(b3[(W(0x40)).idx()]))
        ^ W(0x77))
    .u8();
    b3[(W(0x6c)).idx()] = (W(0xee)
        & ((((W::from(b3[(W(0x28)).idx()]) & !W::from(b2[(W(0x8)).idx()]))
            | (W::from(b1[(W(0x23)).idx()]) & W::from(b2[(W(0x8)).idx()])))
            & W::from(b3[(W(0x40)).idx()]))
            << W(0x1)))
    .u8();
    b3[(W(0x70)).idx()] =
        ((!W::from(b3[(W(0x40)).idx()]) & (W::from(b3[(W(0x54)).idx()]) / W(0x3))) ^ W(0x31)).u8();
    b3[(W(0x74)).idx()] = (W(0x62)
        & ((!W::from(b3[(W(0x40)).idx()]) & (W::from(b3[(W(0x54)).idx()]) / W(0x3))) << W(0x1)))
    .u8();
    a = (W::from(b1[(W(0x23)).idx()]) & W::from(b2[(W(0x8)).idx()]))
        | (W::from(b3[(W(0x28)).idx()]) & !W::from(b2[(W(0x8)).idx()]));
    b = (a & W::from(b3[(W(0x40)).idx()]))
        | ((W::from(b3[(W(0x54)).idx()]) / W(0x3)) & !W::from(b3[(W(0x40)).idx()]));
    b1[(W(0x8f)).idx()] = (W::from(b3[(W(0x60)).idx()])
        - ((b & (W(0x56) + ((W::from(b1[(W(0xac)).idx()]) & W(0x40)) >> W(0x1))))
            | (((((W::from(b1[(W(0xac)).idx()]) & W(0x41)) >> W(0x1)) ^ W(0x56))
                | ((!W::from(b3[(W(0x40)).idx()]) & (W::from(b3[(W(0x54)).idx()]) / W(0x3)))
                    | (((W::from(b3[(W(0x28)).idx()]) & !W::from(b2[(W(0x8)).idx()]))
                        | (W::from(b1[(W(0x23)).idx()]) & W::from(b2[(W(0x8)).idx()])))
                        & W::from(b3[(W(0x40)).idx()]))))
                & W::from(b3[(W(0x64)).idx()]))))
    .u8();
    b2[(W(0x1d)).idx()] = (W(0xa2)).u8();
    a = ((((W::from(b4[(W::from(b3[(W(0x58)).idx()]) % W(0x15)).idx()])) & W(0xa0))
        | (W::from(b0[(W::from(b1[(W(0x7d)).idx()]) % W(0x14)).idx()]) & W(0x5f)))
        >> W(0x1));
    b = W::from(b2[(W::from(b1[(W(0x95)).idx()]) % W(0x23)).idx()])
        ^ (W::from(b1[(W(0x2b)).idx()]) * W::from(b1[(W(0x2b)).idx()]));
    b0[(W(0xf)).idx()] = (W::from(b0[(W(0xf)).idx()]) + ((b & a) | ((a | b) & W(0x73)))).u8();
    b3[(W(0x78)).idx()] = (W::from(b3[(W(0x40)).idx()])
        - W::from(b0[(W::from(b3[(W(0x28)).idx()]) % W(0x14)).idx()]))
    .u8();
    b1[(W(0x5f)).idx()] = (W::from(b4[(W::from(b3[(W(0x14)).idx()]) % W(0x15)).idx()])).u8();
    a = weird_ror8(
        W::from(b2[(W::from(b3[(W(0x50)).idx()]) % W(0x23)).idx()]),
        (W::from(b2[(W::from(b1[(W(0x11)).idx()]) % W(0x23)).idx()])
            * W::from(b2[(W::from(b1[(W(0x11)).idx()]) % W(0x23)).idx()])
            * W::from(b2[(W::from(b1[(W(0x11)).idx()]) % W(0x23)).idx()]))
            & W(0x7),
    );
    b0[(W(0x7)).idx()] = (W::from(b0[(W(0x7)).idx()]) - (a * a)).u8();
    b2[(W(0x8)).idx()] = (W::from(b2[(W(0x8)).idx()]) - W::from(b1[(W(0xb8)).idx()])
        + (W::from(b4[(W::from(b1[(W(0xca)).idx()]) % W(0x15)).idx()])
            * W::from(b4[(W::from(b1[(W(0xca)).idx()]) % W(0x15)).idx()])
            * W::from(b4[(W::from(b1[(W(0xca)).idx()]) % W(0x15)).idx()])))
    .u8();
    b0[(W(0x10)).idx()] =
        ((W::from(b2[(W::from(b1[(W(0x66)).idx()]) % W(0x23)).idx()]) << W(0x1)) & W(0x84)).u8();
    b3[(W(0x7c)).idx()] = ((W::from(b4[(W::from(b3[(W(0x28)).idx()]) % W(0x15)).idx()]) >> W(0x1))
        ^ W::from(b3[(W(0x44)).idx()]))
    .u8();
    b0[(W(0x7)).idx()] = (W::from(b0[(W(0x7)).idx()])
        - (W::from(b0[(W::from(b1[(W(0xbf)).idx()]) % W(0x14)).idx()])
            - (((W::from(b4[(W::from(b1[(W(0x50)).idx()]) % W(0x15)).idx()]) << W(0x1))
                & !W(0xb1))
                | (W::from(
                    b4[(W::from(b4[(W::from(b3[(W(0x58)).idx()]) % W(0x15)).idx()]) % W(0x15))
                        .idx()],
                ) & W(0xb1)))))
    .u8();
    b0[(W(0x6)).idx()] = (W::from(b0[(W::from(b1[(W(0x77)).idx()]) % W(0x14)).idx()])).u8();
    a = (W::from(b4[(W::from(b1[(W(0xbe)).idx()]) % W(0x15)).idx()]) & !W(0xd1))
        | (W::from(b1[(W(0x76)).idx()]) & W(0xd1));
    b = W::from(b0[(W::from(b3[(W(0x78)).idx()]) % W(0x14)).idx()])
        * W::from(b0[(W::from(b3[(W(0x78)).idx()]) % W(0x14)).idx()]);
    b0[(W(0xc)).idx()] = ((W::from(b0[(W::from(b3[(W(0x54)).idx()]) % W(0x14)).idx()])
        ^ (W::from(b2[(W::from(b1[(W(0x47)).idx()]) % W(0x23)).idx()])
            + W::from(b2[(W::from(b1[(W(0xf)).idx()]) % W(0x23)).idx()])))
        & ((a & b) | ((a | b) & W(0x1b))))
    .u8();
    b = (W::from(b1[(W(0x20)).idx()])
        & W::from(b2[(W::from(b3[(W(0x58)).idx()]) % W(0x23)).idx()]))
        | ((W::from(b1[(W(0x20)).idx()])
            | W::from(b2[(W::from(b3[(W(0x58)).idx()]) % W(0x23)).idx()]))
            & W(0x17));
    d = (((W::from(b4[(W::from(b1[(W(0x39)).idx()]) % W(0x15)).idx()]) * W(0xe7)) & W(0xa9))
        | (b & W(0x56)));
    f = (((W::from(b0[(W::from(b1[(W(0x52)).idx()]) % W(0x14)).idx()]) & !W(0x1d))
        | (W::from(b4[(W::from(b3[(W(0x7c)).idx()]) % W(0x15)).idx()]) & W(0x1d)))
        & W(0xbe))
        | (W::from(b4[((d / W(0x5)) % W(0x15)).idx()]) & !W(0xbe));
    h = W::from(b0[(W::from(b3[(W(0x28)).idx()]) % W(0x14)).idx()])
        * W::from(b0[(W::from(b3[(W(0x28)).idx()]) % W(0x14)).idx()])
        * W::from(b0[(W::from(b3[(W(0x28)).idx()]) % W(0x14)).idx()]);
    k = (h & W::from(b1[(W(0x52)).idx()]))
        | (h & W(0x5c))
        | (W::from(b1[(W(0x52)).idx()]) & W(0x5c));
    b3[(W(0x80)).idx()] = (((f & k) | ((f | k) & W(0xc0))) ^ (d / W(0x5))).u8();
    b2[(W(0x19)).idx()] = (W::from(b2[(W(0x19)).idx()])
        ^ (((W::from(b0[(W::from(b3[(W(0x78)).idx()]) % W(0x14)).idx()]) << W(0x1))
            * W::from(b1[(W(0x5)).idx()]))
            - (weird_rol8(
                W::from(b3[(W(0x4c)).idx()]),
                (W::from(b4[(W::from(b3[(W(0x7c)).idx()]) % W(0x15)).idx()]) & W(0x7)),
            ) & (W::from(b3[(W(0x14)).idx()]) + W(0x6e)))))
    .u8();
}
