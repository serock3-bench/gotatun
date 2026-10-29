// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// This file incorporates work covered by the following copyright and
// permission notice:
//
//   Copyright (c) Mullvad VPN AB. All rights reserved.
//   Copyright (c) 2019 Cloudflare, Inc. All rights reserved.
//
// SPDX-License-Identifier: MPL-2.0
use blake2s_benching::{bench_blake2s_hash, bench_blake2s_hmac, bench_blake2s_keyed};
use chacha20poly1305_benching::bench_chacha20poly1305;
use hotpath::criterion::{criterion_group, criterion_main};

mod blake2s_benching;
mod chacha20poly1305_benching;

criterion_group!(
    crypto_benches,
    bench_chacha20poly1305,
    bench_blake2s_hash,
    bench_blake2s_hmac,
    bench_blake2s_keyed,
);
criterion_main!(allocator = none; crypto_benches);
