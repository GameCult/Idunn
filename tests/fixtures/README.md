# Bifrost release-authority wire fixture

`bifrost-release-authority-v1.payload.hex` is the exact MessagePack payload
emitted on 2026-07-16 by Bifrost's TypeScript CultCache writer
`tools/bifrost-repository-release-authority.mjs` for `GameCult/Epiphany`,
`refs/heads/main`, commit
`0123456789abcdef0123456789abcdef01234567`.

It is checked in because the TypeScript runtime emits a camelCase MessagePack
map while Rust `DatabaseEntry` derives tuple payloads. Idunn must prove it can
decode the producer's real wire shape rather than round-tripping a fixture it
serialized itself.

# systemd 257 `systemctl show` captures

`systemd-257/*.show` are the verbatim output of systemd 257.13 for the 27
properties `show_unit` requested at `112a7a5`, captured read-only on Yggdrasil
on 2026-10-24: `vili-presence-identity-only.show` (a failed idunn-vili unit whose
only credential is the presence identity) and
`odin-presence-and-one-service-credential.show` (the live Odin unit). systemd 257
prints `LoadCredential=[unprintable]` in both. Tests read these bytes; do not edit.

# Pre-cut systemd observation records

`pre-cut-systemd-observation.{compact,named}.hex` are the two MessagePack
encodings of a `SystemdWorkloadObservation` with `load_credential` set to
`[unprintable]`, produced by the code at `112a7a5` (the form live incumbents
persisted). They pin that later Idunns still decode such records.
