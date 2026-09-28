use super::*;
use aven_core::sync::encrypted_tail::batch::Envelope;
use aven_core::sync::encrypted_tail::{BatchOperation, BatchReply};

pub(super) async fn handle(State(server): State<Arc<Server>>, request: Request) -> Response {
    let server = &*server;
    let outcome = http_admission::dispatch(
        &server.gate,
        REQUEST_TIMEOUT,
        request,
        tail::BATCH_APPEND_LIMIT,
        |headers, bytes| async move {
            let result = dispatch(&server.db, headers, bytes).await;
            match result {
                Ok(reply) => http_admission::reply(&CODES, &reply, tail::BATCH_CONTROL_LIMIT),
                Err(error)
                    if aven_core::sync::client::errors::has_code(
                        &error,
                        "encrypted-tail-batch-known",
                    ) =>
                {
                    http_admission::refusal(StatusCode::CONFLICT, "encrypted-tail-batch-known")
                }
                Err(error) if error.is::<tail::PrefixIdentityCollision>() => {
                    http_admission::refusal(
                        StatusCode::CONFLICT,
                        "encrypted-tail-prefix-identity-collision",
                    )
                }
                Err(error) => http_admission::operation_refusal(&CODES, &error),
            }
        },
    )
    .await;
    http_admission::respond(&CODES, outcome)
}

async fn dispatch(
    db: &Database,
    headers: HeaderMap,
    bytes: Option<Bytes>,
) -> Result<Envelope<BatchReply>> {
    let bytes = CODES.json_body(&headers, bytes)?;
    let bearer = CODES.bearer(&headers)?;
    let input: Envelope<BatchOperation> = CODES.parse(&bytes)?;
    if matches!(input.operation, BatchOperation::Resolve { .. })
        && bytes.len() > tail::BATCH_CONTROL_LIMIT
    {
        return Err(CODES.too_large().into());
    }
    let operation = db
        .encrypted_tail_batch_exchange(&input.context, &bearer, input.operation)
        .await?;
    Ok(Envelope {
        context: input.context,
        correlation: input.correlation,
        operation,
    })
}
