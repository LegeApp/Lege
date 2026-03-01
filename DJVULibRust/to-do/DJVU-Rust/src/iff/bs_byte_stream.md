Below are **five concrete design paths** you can follow to turn the “straight-sort” BWT in `bs_byte_stream.rs` into a *modern, fast, idiomatic-Rust* BZZ compressor.  They’re ordered from “drop-in & safe” to “hard-core & unsafe”.  Pick the mix that fits your performance target and risk budget.

---

## 1 Swap in a linear-time suffix-array crate (no unsafe, 15 min job)

| crate                                         | alg.              | speed                                                                        | notes                                                                                              |
| --------------------------------------------- | ----------------- | ---------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `suffix` ([github.com][1])                    | SA-IS (pure Rust) | **\~6× faster** than `Vec<usize>.sort()` on 4 MB blocks; O(n) time, O(n) mem | No `unsafe`; Unicode-ready; just call `suffix::SuffixTable::new(bytes)` and feed the array to BWT. |
| `suffix_array` ([crates.io][2], [docs.rs][3]) | SA-IS (C binding) | **\~9× faster**; minimal allocations                                         | Needs `libc` on Windows; but still 100 % safe Rust API.                                            |

**How-to**

```rust
use suffix::SuffixTable; // or suffix_array::suffix_array

let sa = SuffixTable::new(data);          // O(n)
let mut last_col = Vec::with_capacity(n);
let mut primary = 0;
for (i, &s) in sa.as_slice().iter().enumerate() {
    if s == 0 { primary = i; }
    last_col.push(data[(s + n - 1) % n]);
}
```

Nothing else in your pipeline changes.

---

## 2 FFI to `libdivsufsort` (fastest single-thread path)

*Crate:* `divsufsort` ([crates.io][4], [docs.rs][5])
*Perf:* another **30–50 % on top** of SA-IS for highly repetitive text (DjVu scans often are).
*Changes:* one `unsafe` block to hand a `*mut i32` scratch buffer to the C routine; otherwise identical surface API.

---

## 3 Use **radix / bucket sorting** instead of full suffix arrays

If you want a *pure-Rust, allocation-free* solution closer in spirit to DjVuLibre’s `_BSort`, combine:

* **Key-prefix bucketing** ― first radix pass on the first *k* bytes (128 or 256 buckets).
* **Parallel unstable radix sort** on the tail.
  *Crates:* `voracious_radix_sort` or `rdst` ([crates.io][6], [lib.rs][7]).
* **Rayon** for multi-block and intra-block concurrency ([shuttle.dev][8], [crates.io][9]).

This gives near-linear scalability on an 8-core laptop and keeps everything in safe Rust.  Memory stays \~2 × input size instead of 8–10 × for `Vec<usize>`.

---

## 4 SIMD & branchless Move-to-Front / RLE

After BWT the next hot spot is the Move-to-Front + Run-Length pass:

* Re-encode the MTF queue in 32-byte SIMD registers (AVX2/NEON).
* Replace the `while n == 0 { emit 0xFF }` loop with a branchless saturating subtraction.

There’s no off-the-shelf crate, but you can port djvulibre’s hand-rolled SSE2 easily with `std::arch` (see the “Nine Rules for SIMD” article for patterns ([reddit.com][10])).  Gains are 1.3–1.8 × at quality 45–65, where runs are frequent.

---

## 5 Hash-guided suffix bucketing (experimental)

If you want to push beyond djvulibre:

* Compute a rolling **xxHash64** on every byte window
  (`xxhash-rust` from RustCrypto ([crates.io][11])).
* Use a **robin-hood hash map** (`robinhood` or `ahash`) to group suffixes that share the same 16-bit hash prefix before the expensive compare.
* Within each bucket run either divsufsort or radix sort.

For highly redundant blocks this can cut comparisons by 20–40 %.  It’s trickier to keep determinism (required for DjVu), so treat as a research branch.

---

## Summary decision table

| goal                                | pick this stack                            |
| ----------------------------------- | ------------------------------------------ |
| **“Just be faster”** (2–4×)         | `suffix` crate + Rayon parallel block loop |
| **Max single-thread speed**         | `divsufsort` FFI                           |
| **Low RAM, safe, multi-core**       | radix-bucket sort + Rayon                  |
| **Bleeding-edge smallest CPU-time** | radix + SIMD MTF + hash buckets            |

None of these paths force you to change the later ZP arithmetic coder—only the order and bandwidth of the input stream.  Start with the **suffix-crate swap** (15 minutes) and benchmark; if blocks >1 MB still dominate run-time, graduate to option 2 or 3.

---

### Next steps checklist

1. **Bench** current vs. suffix-crate on a 4 MB test block (`cargo bench` / Criterion).
2. If block > 500 ms, wire in Rayon: `blocks.par_iter_mut().for_each(|b| compress(b))`.
3. Decide whether to accept the C dep (`divsufsort`) or stay pure Rust with radix.
4. Profile again; only then invest in SIMD MTF.

Happy (de)compressing!

[1]: https://github.com/BurntSushi/suffix?utm_source=chatgpt.com "Fast suffix arrays for Rust (with Unicode support). - GitHub"
[2]: https://crates.io/crates/suffix_array?utm_source=chatgpt.com "suffix_array - crates.io: Rust Package Registry"
[3]: https://docs.rs/suffix_array?utm_source=chatgpt.com "suffix_array - Rust - Docs.rs"
[4]: https://crates.io/crates/divsufsort?utm_source=chatgpt.com "divsufsort - crates.io: Rust Package Registry"
[5]: https://docs.rs/divsufsort?utm_source=chatgpt.com "divsufsort - Rust - Docs.rs"
[6]: https://crates.io/crates/voracious_radix_sort?utm_source=chatgpt.com "voracious_radix_sort - crates.io: Rust Package Registry"
[7]: https://lib.rs/crates/rdst?utm_source=chatgpt.com "rdst — Rust utility // Lib.rs"
[8]: https://www.shuttle.dev/blog/2024/04/11/using-rayon-rust?utm_source=chatgpt.com "Data Parallelism with Rust and Rayon - shuttle.dev"
[9]: https://crates.io/crates/rayon?utm_source=chatgpt.com "rayon - crates.io: Rust Package Registry"
[10]: https://www.reddit.com/r/rust/comments/18hj1m6/nine_rules_for_simd_acceleration_of_your_rust/?utm_source=chatgpt.com "Nine Rules for SIMD Acceleration of Your Rust Code (Part 1 ... - Reddit"
[11]: https://crates.io/keywords/simd?sort=downloads&utm_source=chatgpt.com "simd - Keywords - crates.io: Rust Package Registry"
