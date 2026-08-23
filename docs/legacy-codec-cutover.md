# Legacy codec repository cutover

The canonical sources are the three directories under
[`lege-codecs/`](../lege-codecs):

- [DJVULibRust](https://github.com/LegeApp/Lege/tree/main/lege-codecs/djvulibrust)
- [jbig2enc-rust](https://github.com/LegeApp/Lege/tree/main/lege-codecs/jbig2enc-rust)
- [jp2lam](https://github.com/LegeApp/Lege/tree/main/lege-codecs/jp2lam)

The standalone repositories are not mirrors. After the monorepo release has
been pushed, run the redirect cutover once per codec from a trusted machine:

```sh
scripts/release/cutover-legacy-codec.sh --codec djvulibrust --apply
scripts/release/cutover-legacy-codec.sh --codec jbig2enc-rust --apply
scripts/release/cutover-legacy-codec.sh --codec jp2lam --apply
```

Each invocation force-replaces only the legacy repository's `main` branch with
a redirect README, sets its GitHub homepage/description, and archives it. It
does not delete tags or existing GitHub releases.

Cargo consumers should use released crate versions, not a legacy Git URL. The
first monorepo-published `jbig2enc-rust` release is `0.5.4`.
