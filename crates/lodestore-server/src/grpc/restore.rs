use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use lodestore_format::crypto::{BlobId, Key32};
use lodestore_format::index::parse_hex32;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::selection::Selection;
use lodestore_format::store::{BlobError, BlobStore, Store};
use lodestore_format::tree::{PlanStep, SelectionWalk};
use lodestore_proto::agent::v1::plan_item::Kind;
use lodestore_proto::agent::v1::{
    ChunkLoc, PackAccess, PackBytes, PlanChunks, PlanEnter, PlanEntry, PlanItem, PlanLeave,
    ReadPackRangeRequest, RefreshPackAccessRequest, RefreshPackAccessResponse, RestorePlanBatch,
    RestoreTotals,
};
use prost::Message;
use sqlx::Row;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use crate::catalog::{self, RepositoryUnavailable};
use crate::grpc::internal;
use crate::grpc::registry::Registry;
use crate::jobs::restore::RestoreRecord;
use crate::repos::checks::open_store;
use crate::repos::db::server_access_checked;
use crate::state::AppState;

pub const PRESIGN_TTL: Duration = Duration::from_secs(3600);
pub const BATCH_LIMIT: usize = 1 << 20;
pub const PACK_PART: usize = 1 << 20;
pub const MAX_CHUNKS_PER_ITEM: usize = 4096;
pub const MAX_PACKS_PER_ITEM: usize = 256;
const MAX_RANGE: u64 = 32 << 20;
const MAX_REFRESH: usize = 4096;
const ACCESS_COST: usize = 1024;
const STREAM_DEPTH: usize = 4;

fn denied() -> Status {
    Status::permission_denied("this job is not an active restore of this agent")
}

fn read_error(error: lodestore_format::Error) -> Status {
    use lodestore_format::Error;
    match error {
        Error::Corrupt { .. } | Error::Store(BlobError::NotFound) => {
            tracing::warn!(%error, "a restore plan met damaged or missing data");
            Status::data_loss("the snapshot data is damaged or missing")
        }
        Error::WalkLimit { .. } => {
            tracing::warn!(%error, "a restore plan hit the tree walk limit");
            Status::resource_exhausted(error.to_string())
        }
        error => internal(error),
    }
}

async fn authorize(
    state: &AppState,
    agent_id: &str,
    job_id: &str,
) -> Result<RestoreRecord, Status> {
    if state.registry.is_revoked(agent_id) {
        return Err(denied());
    }
    let row = sqlx::query(
        "SELECT restore_json FROM jobs WHERE id = $1 AND host_id = $2 AND kind = 'restore' \
         AND state IN ('assigned', 'running')",
    )
    .bind(job_id)
    .bind(agent_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or_else(denied)?;
    let json: Option<String> = row.try_get("restore_json").map_err(internal)?;
    serde_json::from_str(&json.ok_or_else(denied)?).map_err(internal)
}

async fn source_store(state: &AppState, repository_id: &str) -> Result<Store, Status> {
    let access = server_access_checked(state, repository_id)
        .await
        .map_err(
            |error| match error.downcast_ref::<RepositoryUnavailable>() {
                Some(_) => Status::failed_precondition("the source repository is unavailable"),
                None => internal(error),
            },
        )?;
    open_store(&access.location, access.server_creds.as_ref()).map_err(internal)
}

async fn pack_access(
    store: &Store,
    number: u32,
    pack_id: &BlobId,
    key: &Key32,
) -> Result<PackAccess, Status> {
    let presigned = store
        .presign_read(&pack_object_key(pack_id), PRESIGN_TTL)
        .await
        .map_err(internal)?;
    let (url, expires_at_ms) = presigned
        .map(|p| (p.url, p.expires_at_ms))
        .unwrap_or_default();
    Ok(PackAccess {
        pack: number,
        pack_id: pack_id.to_vec(),
        key: key.to_vec(),
        url,
        expires_at_ms,
    })
}

type Sender = mpsc::Sender<Result<RestorePlanBatch, Status>>;

/// Whether a piece holding `chunks` chunks from the packs `used` is full
/// before a chunk of `pack` is added.
fn must_cut(chunks: usize, used: &BTreeSet<u32>, pack: u32) -> bool {
    chunks >= MAX_CHUNKS_PER_ITEM || (used.len() >= MAX_PACKS_PER_ITEM && !used.contains(&pack))
}

struct Planner {
    reader: Reader<Store>,
    tx: Sender,
    registry: Registry,
    job_id: String,
    generation: u64,
    numbers: HashMap<BlobId, u32>,
    packs: Vec<(BlobId, Key32)>,
    batch: RestorePlanBatch,
    in_batch: HashSet<u32>,
    size: usize,
    sent: bool,
}

impl Planner {
    async fn pack_number(&mut self, pack_id: BlobId) -> Result<u32, Status> {
        if let Some(number) = self.numbers.get(&pack_id) {
            return Ok(*number);
        }
        let key = self.reader.pack_key(&pack_id).await.map_err(read_error)?;
        let number = u32::try_from(self.packs.len())
            .map_err(|_| Status::resource_exhausted("too many packs in one restore"))?;
        if !self
            .registry
            .issue_pack(&self.job_id, self.generation, number, pack_id, key.clone())
        {
            return Err(Status::aborted("the plan was requested again"));
        }
        self.packs.push((pack_id, key));
        self.numbers.insert(pack_id, number);
        Ok(number)
    }

    /// Sends the batch; `last` marks the end of the plan and is always sent.
    async fn flush(&mut self, last: bool) -> Result<(), Status> {
        if self.batch.items.is_empty() && self.sent && !last {
            return Ok(());
        }
        let mut batch = std::mem::take(&mut self.batch);
        batch.last = last;
        self.in_batch.clear();
        self.size = 0;
        self.sent = true;
        self.tx
            .send(Ok(batch))
            .await
            .map_err(|_| Status::cancelled("the plan stream was closed"))
    }

    async fn push(&mut self, kind: Kind, packs: &BTreeSet<u32>) -> Result<(), Status> {
        let item = PlanItem { kind: Some(kind) };
        let missing = packs.iter().filter(|n| !self.in_batch.contains(n)).count();
        let cost = item.encoded_len() + missing * ACCESS_COST;
        if self.size > 0 && self.size + cost > BATCH_LIMIT {
            self.flush(false).await?;
        }
        for &number in packs {
            if self.in_batch.contains(&number) {
                continue;
            }
            let (pack_id, key) = &self.packs[number as usize];
            let access = pack_access(self.reader.store(), number, pack_id, key).await?;
            self.size += access.encoded_len() + 3;
            self.in_batch.insert(number);
            self.batch.packs.push(access);
        }
        self.size += item.encoded_len() + 3;
        self.batch.items.push(item);
        Ok(())
    }

    /// Sends a file as one entry item followed by chunk items, each within
    /// [`MAX_CHUNKS_PER_ITEM`] chunks and [`MAX_PACKS_PER_ITEM`] packs.
    async fn entry(&mut self, mut node: pb::Node) -> Result<(), Status> {
        let ids = std::mem::take(&mut node.content);
        node.subtrees.clear();
        let mut head = Some(node.encode_to_vec());
        let mut chunks = Vec::new();
        let mut used = BTreeSet::new();
        for window in ids.chunks(MAX_CHUNKS_PER_ITEM) {
            let probe = pb::Node {
                content: window.to_vec(),
                ..Default::default()
            };
            let locations = self
                .reader
                .chunk_locations(&probe)
                .await
                .map_err(read_error)?;
            for (id, location) in window.iter().zip(&locations) {
                let pack = self.pack_number(location.pack_id).await?;
                if must_cut(chunks.len(), &used, pack) {
                    let piece = (std::mem::take(&mut chunks), std::mem::take(&mut used));
                    self.piece(&mut head, piece, true).await?;
                }
                used.insert(pack);
                chunks.push(ChunkLoc {
                    pack,
                    offset: location.offset,
                    enc_len: location.enc_len,
                    ulen: location.ulen,
                    id: id.clone(),
                });
            }
        }
        self.piece(&mut head, (chunks, used), false).await
    }

    async fn piece(
        &mut self,
        head: &mut Option<Vec<u8>>,
        (chunks, used): (Vec<ChunkLoc>, BTreeSet<u32>),
        more: bool,
    ) -> Result<(), Status> {
        let kind = match head.take() {
            Some(node) => Kind::Entry(PlanEntry { node, chunks, more }),
            None => Kind::Chunks(PlanChunks { chunks, more }),
        };
        self.push(kind, &used).await
    }

    async fn run(mut self, snapshot_id: BlobId, selection: Selection) -> Result<(), Status> {
        let totals = self
            .reader
            .selection_totals(&snapshot_id, &selection)
            .await
            .map_err(read_error)?;
        self.batch.totals = Some(RestoreTotals {
            files: totals.files,
            bytes: totals.bytes,
        });
        let mut walk = SelectionWalk::start(&mut self.reader, &snapshot_id, &selection)
            .await
            .map_err(read_error)?;
        let none = BTreeSet::new();
        while let Some(step) = walk.next(&mut self.reader).await.map_err(read_error)? {
            match step {
                PlanStep::Enter { mut node, ancestor } => {
                    node.subtrees.clear();
                    let enter = PlanEnter {
                        node: node.encode_to_vec(),
                        ancestor,
                    };
                    self.push(Kind::Enter(enter), &none).await?;
                }
                PlanStep::Entry(node) => self.entry(node).await?,
                PlanStep::Leave => self.push(Kind::Leave(PlanLeave {}), &none).await?,
            }
        }
        self.flush(true).await
    }
}

pub async fn restore_plan(
    state: &AppState,
    agent_id: &str,
    job_id: &str,
) -> Result<ReceiverStream<Result<RestorePlanBatch, Status>>, Status> {
    let record = authorize(state, agent_id, job_id).await?;
    let selection = record
        .items()
        .ok_or_else(|| internal("the stored selection is not base64"))
        .and_then(|items| Selection::from_paths(items).map_err(internal))?;
    let snapshot_id =
        parse_hex32(&record.snapshot_id).ok_or_else(|| internal("the stored snapshot id"))?;
    let source = catalog::open_repository(state, &record.source_repository_id)
        .await
        .map_err(internal)?;
    let reader = Reader::open(source.store, source.keys, &source.config).map_err(read_error)?;
    let generation = state.registry.begin_plan(job_id);
    let (tx, rx) = mpsc::channel(STREAM_DEPTH);
    let planner = Planner {
        reader,
        tx: tx.clone(),
        registry: state.registry.clone(),
        job_id: job_id.to_string(),
        generation,
        numbers: HashMap::new(),
        packs: Vec::new(),
        batch: RestorePlanBatch::default(),
        in_batch: HashSet::new(),
        size: 0,
        sent: false,
    };
    let task = tokio::spawn(planner.run(snapshot_id, selection));
    let job_id = job_id.to_string();
    tokio::spawn(async move {
        let status = match task.await {
            Ok(Ok(())) => return,
            Ok(Err(status)) => status,
            Err(error) => {
                tracing::error!(%error, job_id = %job_id, "the restore planner stopped");
                Status::internal("the restore plan stopped unexpectedly")
            }
        };
        let _ = tx.send(Err(status)).await;
    });
    Ok(ReceiverStream::new(rx))
}

pub async fn refresh_pack_access(
    state: &AppState,
    agent_id: &str,
    request: RefreshPackAccessRequest,
) -> Result<RefreshPackAccessResponse, Status> {
    let record = authorize(state, agent_id, &request.job_id).await?;
    if request.packs.len() > MAX_REFRESH {
        return Err(Status::invalid_argument("too many packs in one request"));
    }
    let numbers: BTreeSet<u32> = request.packs.iter().copied().collect();
    let mut issued = Vec::with_capacity(numbers.len());
    for number in numbers {
        let (pack_id, key) = state
            .registry
            .issued_pack(&request.job_id, number)
            .ok_or_else(|| Status::permission_denied("the pack was not issued to this job"))?;
        issued.push((number, pack_id, key));
    }
    if issued.is_empty() {
        return Ok(RefreshPackAccessResponse::default());
    }
    let store = source_store(state, &record.source_repository_id).await?;
    let mut packs = Vec::with_capacity(issued.len());
    for (number, pack_id, key) in &issued {
        packs.push(pack_access(&store, *number, pack_id, key).await?);
    }
    Ok(RefreshPackAccessResponse { packs })
}

pub async fn read_pack_range(
    state: &AppState,
    agent_id: &str,
    request: ReadPackRangeRequest,
) -> Result<ReceiverStream<Result<PackBytes, Status>>, Status> {
    let record = authorize(state, agent_id, &request.job_id).await?;
    let length = u64::from(request.length);
    if length == 0 || length > MAX_RANGE {
        return Err(Status::invalid_argument(
            "the range length is out of bounds",
        ));
    }
    let end = request
        .offset
        .checked_add(length)
        .ok_or_else(|| Status::invalid_argument("the range is out of bounds"))?;
    let (pack_id, _) = state
        .registry
        .issued_pack(&request.job_id, request.pack)
        .ok_or_else(|| Status::permission_denied("the pack was not issued to this job"))?;
    let store = source_store(state, &record.source_repository_id).await?;
    let bytes = store
        .get_range(&pack_object_key(&pack_id), request.offset..end)
        .await
        .map_err(|error| match error {
            BlobError::NotFound => Status::not_found("the pack is missing"),
            error => internal(error),
        })?;
    if bytes.len() as u64 != length {
        return Err(Status::out_of_range("the range reaches past the pack"));
    }
    let (tx, rx) = mpsc::channel(2);
    tokio::spawn(async move {
        let mut at = 0;
        while at < bytes.len() {
            let to = (at + PACK_PART).min(bytes.len());
            let part = PackBytes {
                data: bytes.slice(at..to).to_vec(),
            };
            if tx.send(Ok(part)).await.is_err() {
                return;
            }
            at = to;
        }
    });
    Ok(ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_limit_fails_the_restore_plan() {
        let status = read_error(lodestore_format::Error::WalkLimit {
            what: "entries",
            limit: 7,
        });
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert!(status.message().contains('7'));
    }

    #[test]
    fn a_piece_is_cut_by_chunks_and_by_new_packs() {
        let full: BTreeSet<u32> = (0..MAX_PACKS_PER_ITEM as u32).collect();
        assert!(!must_cut(0, &BTreeSet::new(), 0));
        assert!(!must_cut(MAX_CHUNKS_PER_ITEM - 1, &BTreeSet::from([0]), 0));
        assert!(must_cut(MAX_CHUNKS_PER_ITEM, &BTreeSet::from([0]), 0));
        assert!(!must_cut(10, &full, 0), "a pack already in the piece");
        assert!(
            must_cut(10, &full, MAX_PACKS_PER_ITEM as u32),
            "one pack too many"
        );
    }
}
