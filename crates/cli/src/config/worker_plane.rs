//! `config::worker_plane`: schema domain of the daemon config.

use super::*;

/// The additive `[worker_plane]` section: the DEPLOYMENT BOUNDARY of the
/// remote/VPC worker HTTP surface — a SECOND listener with its OWN identity,
/// separate from the loopback-oriented native listener.
///
/// Strict and additive:
///
/// - `enabled` (default `false`): while false the daemon binds no second
///   socket, the native listener stays exactly as before and the worker
///   routes keep their existing behavior (disabled parity);
/// - `bind` (default `127.0.0.1:8790`): the dedicated worker-plane socket.
///   The native listener's bind is NOT configurable (always loopback), so
///   the two listeners can never share exposure. A non-loopback bind is
///   refused at startup with a typed refusal NAMING the deployment boundary
///   unless `trusted_gateway = true` acknowledges an external
///   TLS-terminating gateway fronting the socket;
/// - `tls` (default `false`): request IN-PROCESS TLS termination. This
///   workspace compiles no inbound TLS stack (rustls exists only as a
///   transitive outbound HTTP-client dependency), so `tls = true` is a
///   typed startup refusal — never fabricated. Terminate TLS at the trusted
///   gateway and use the gateway-only mode;
/// - `trusted_gateway` (default `false`): the explicit acknowledgement
///   naming the boundary: an external, trusted gateway terminates TLS (and,
///   under `auth = "gateway_mtls"`, client mTLS) in front of this socket.
///   It is required for any non-loopback bind and recorded in the daemon's
///   startup audit line and by `faktor doctor --config`;
/// - `auth` (default `"worker_tokens"`): the plane's OWN transport
///   client-auth mode. `"gateway_mtls"` requires `trusted_gateway = true`
///   and a `bearer` (mTLS termination exists only at the gateway);
/// - `bearer`: an optional transport credential required as
///   `Authorization: Bearer <bearer>` on EVERY worker-plane request, in
///   addition to the worker registration token. The daemon password is
///   never accepted on the worker socket.
///
/// Enabling the section requires `[workers] enabled = true` (there is no
/// plane to expose otherwise); the pair is refused at config load. `Debug`
/// redacts the transport bearer, and the bearer is a
/// [`SecretValue`](faktor_security::secret::SecretValue) — no `Display`, no
/// serde — so the only reader is the constant-time worker-plane header
/// check.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct WorkerPlaneCfg {
    pub enabled: bool,
    pub bind: Option<String>,
    pub tls: bool,
    pub trusted_gateway: bool,
    pub auth: Option<String>,
    pub bearer: Option<SecretValue>,
}

impl serde::Serialize for WorkerPlaneCfg {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut out = serializer.serialize_struct("WorkerPlaneCfg", 6)?;
        out.serialize_field("enabled", &self.enabled)?;
        out.serialize_field("bind", &self.bind)?;
        out.serialize_field("tls", &self.tls)?;
        out.serialize_field("trusted_gateway", &self.trusted_gateway)?;
        out.serialize_field("auth", &self.auth)?;
        // The transport bearer is a credential with no serde: the saved
        // config projection never writes it (absent and present both render
        // as `null`), so a saved file can never carry the plaintext. A
        // reloaded `gateway_mtls` section refuses typed for the missing
        // bearer — fail closed, never a fabricated credential.
        out.serialize_field("bearer", &Option::<String>::None)?;
        out.end()
    }
}

impl std::fmt::Debug for WorkerPlaneCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPlaneCfg")
            .field("enabled", &self.enabled)
            .field("bind", &self.bind)
            .field("tls", &self.tls)
            .field("trusted_gateway", &self.trusted_gateway)
            .field("auth", &self.auth)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The `[worker_plane]` keys, in stable order (unknown-field errors list
/// them).
pub const WORKER_PLANE_FIELDS: &[&str] = &[
    "enabled",
    "bind",
    "tls",
    "trusted_gateway",
    "auth",
    "bearer",
];

/// The default worker-plane bind: loopback (only an explicit operator bind
/// moves it, and only under the gateway rules).
pub const DEFAULT_WORKER_PLANE_BIND: &str = faktor_server::DEFAULT_WORKER_PLANE_BIND;

impl<'de> serde::Deserialize<'de> for WorkerPlaneCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = WorkerPlaneCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [worker_plane] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<WorkerPlaneCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = WorkerPlaneCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u8, "enabled"),
                        "bind" => (2, "bind"),
                        "tls" => (4, "tls"),
                        "trusted_gateway" => (8, "trusted_gateway"),
                        "auth" => (16, "auth"),
                        "bearer" => (32, "bearer"),
                        other => return Err(A::Error::unknown_field(other, WORKER_PLANE_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.bind = map.next_value::<Option<String>>()?,
                        4 => out.tls = map.next_value::<bool>()?,
                        8 => out.trusted_gateway = map.next_value::<bool>()?,
                        16 => out.auth = map.next_value::<Option<String>>()?,
                        // The wire value is read as text and wrapped (zeroized,
                        // redacted) IMMEDIATELY: no plaintext field is ever
                        // stored on the config type.
                        _ => out.bearer = map.next_value::<Option<String>>()?.map(SecretValue::new),
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl WorkerPlaneCfg {
    /// The parsed bind (shape-checked on both load paths, enabled or not).
    pub fn bind(&self) -> Result<std::net::SocketAddr, String> {
        let raw = self.bind.as_deref().unwrap_or(DEFAULT_WORKER_PLANE_BIND);
        raw.parse()
            .map_err(|_| format!("worker_plane: bind {raw:?} must be a host:port socket address"))
    }

    /// The parsed transport client-auth mode.
    pub fn auth(&self) -> Result<faktor_server::WorkerPlaneAuth, String> {
        match self.auth.as_deref() {
            None | Some("worker_tokens") => Ok(faktor_server::WorkerPlaneAuth::WorkerTokens),
            Some("gateway_mtls") => Ok(faktor_server::WorkerPlaneAuth::GatewayMtls),
            Some(other) => Err(format!(
                "worker_plane: auth {other:?} must be one of worker_tokens|gateway_mtls"
            )),
        }
    }

    /// The resolved worker-plane bind/transport configuration (`None` while
    /// the section is disabled — the daemon then binds no second socket).
    /// Every boundary refusal is the server crate's typed error, rendered
    /// with its stable machine code so startup refusals are unmistakable.
    pub fn resolve(&self) -> Result<Option<faktor_server::WorkerPlaneBindConfig>, String> {
        let bind = self.bind()?;
        let auth = self.auth()?;
        if !self.enabled {
            return Ok(None);
        }
        let config = faktor_server::WorkerPlaneBindConfig {
            bind,
            transport: if self.tls {
                faktor_server::WorkerPlaneTransport::Tls
            } else {
                faktor_server::WorkerPlaneTransport::Plaintext
            },
            trusted_gateway: self.trusted_gateway,
            auth,
            bearer: self.bearer.clone(),
        };
        config
            .validate()
            .map_err(|refusal| format!("worker_plane: [{}] {refusal}", refusal.code()))?;
        Ok(Some(config))
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). Shape errors (bind/auth) are always errors; the deployment
    /// boundary rules apply when the section is enabled.
    pub fn validate(&self) -> Result<(), String> {
        let _ = self.resolve()?;
        Ok(())
    }
}
