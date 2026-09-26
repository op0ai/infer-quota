use quota_core::timeutil::now_unix;
use quota_core::types::{ProviderId, ProviderSnapshot, Snapshot};

use crate::http::Transport;

pub struct ProbeCtx<'a> {
    pub transport: &'a dyn Transport,
    pub now: i64,
}

pub trait Provider {
    fn id(&self) -> ProviderId;
    fn probe(&self, ctx: &ProbeCtx<'_>) -> ProviderSnapshot;
}

pub struct AdapterSet<'a> {
    pub providers: Vec<&'a dyn Provider>,
}

pub fn probe_all(providers: &[&dyn Provider], transport: &dyn Transport) -> Snapshot {
    let now = now_unix();
    let ctx = ProbeCtx { transport, now };
    let snaps = providers.iter().map(|p| p.probe(&ctx)).collect();
    Snapshot::new(now, snaps)
}
