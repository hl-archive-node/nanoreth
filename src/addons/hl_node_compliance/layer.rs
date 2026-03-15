use http::{Extensions, Request};
use std::task::{Context, Poll};
use tower::{Layer, Service};

/// Marker type inserted into HTTP extensions to indicate per-request compliance mode.
#[derive(Debug, Clone, Copy)]
pub struct HlComplianceMode(pub bool);

/// Tower layer that extracts `?hl=true/false` from query string and injects into extensions.
#[derive(Debug, Clone)]
pub struct HlComplianceLayer {
    default_compliant: bool,
}

impl HlComplianceLayer {
    pub fn new(default_compliant: bool) -> Self {
        Self { default_compliant }
    }
}

impl<S> Layer<S> for HlComplianceLayer {
    type Service = HlComplianceService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        HlComplianceService { inner, default_compliant: self.default_compliant }
    }
}

#[derive(Debug, Clone)]
pub struct HlComplianceService<S> {
    inner: S,
    default_compliant: bool,
}

impl<S, B> Service<Request<B>> for HlComplianceService<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let compliant = parse_hl_query(req.uri().query(), self.default_compliant);
        req.extensions_mut().insert(HlComplianceMode(compliant));
        self.inner.call(req)
    }
}

fn parse_hl_query(query: Option<&str>, default: bool) -> bool {
    let Some(query) = query else { return default };
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(key), Some(value)) = (parts.next(), parts.next())
            && key == "hl" {
                return value == "true" || value == "1";
            }
    }
    default
}

/// Check if the current request should use hl-node-compliant mode.
/// Falls back to the `default` when no per-request extension is present.
pub fn is_hl_compliant(extensions: &Extensions, default: bool) -> bool {
    extensions.get::<HlComplianceMode>().map_or(default, |m| m.0)
}
