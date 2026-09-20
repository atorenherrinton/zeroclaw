# Native Docs candidate status

This source adds isolated exact-scope Workspace consent, a terminal/native-owner
single-document grant issuer, a sealed durable journal, and guarded create,
blank-only population and readback. See [README](README.md) for commands, canonical
state ownership, supported content, failure modes and coordinator integration.

The prior restored source was read-only and its interactive doctor had no consent
flow. Existing Gmail/Calendar grants remain canonical for those connectors; the
new Workspace credential is separate. Functional source is independent of any CI
workflow edit. The local native gate is required; CI wiring remains deferred.

Hermetic tests cannot establish Google consent, native owner authentication,
Keychain approval for signed bytes, live tool registration or an actual document.
The coordinator owns PR review/merge, stable signing/install, configuration and
owner interaction. Do not report document completion until exact creation,
population and whole-document readback have succeeded. Never send or share a
result through this helper; it exposes no messaging or permission APIs.

The terminal issuer is a bounded host capability. Integration must deny models
filesystem/administrative access to its private state and issuer. HMAC verification
rejects fabricated or altered journals; restoring an older valid journal remains
outside its threat boundary and must never be used for rollback.
