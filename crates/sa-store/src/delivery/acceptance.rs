use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryAcceptance {
    pub id: i64,
    pub ticket: i64,
    pub assessment_id: i64,
    pub snapshot_id: i64,
    pub contract_revision: i64,
    pub policy_revision: i64,
    pub status: String,
    pub assessment_sha256: String,
    pub created_at: f64,
}

/// The receipt identity includes all unmet evidence, the manifest digest,
/// contract and policy versions, and the exact behavior attempts evaluated.
pub fn acceptance_digest(assessment: &DeliveryAssessment, behavior_basis: &str) -> Result<String> {
    // Keep the v1 field order explicit. Adding optional UI fields to a view
    // must not invalidate a previously recorded operator decision.
    let unmet: Vec<_> = assessment
        .unmet
        .iter()
        .map(|item| {
            (
                item.requirement_id,
                item.requirement_revision,
                &item.status,
                &item.reason,
                &item.source,
            )
        })
        .collect();
    let identity = (
        "operator-acceptance/1",
        assessment.id,
        assessment.ticket,
        assessment.contract_revision,
        assessment.snapshot_id,
        assessment.policy_revision,
        &assessment.manifest_sha256,
        &assessment.status,
        assessment.created_at.to_bits(),
        unmet,
        &assessment.reason,
        behavior_basis,
    );
    Ok(sha256(&serde_json::to_vec(&identity)?))
}

pub(super) fn latest(connection: &Connection, ticket: i64) -> Result<Option<DeliveryAcceptance>> {
    let latest = connection.query_row(
        "SELECT id,assessment_id,assessment_status,assessment_sha256,created FROM delivery_acceptances WHERE ticket=? ORDER BY id DESC LIMIT 1",
        [ticket], |row| Ok(decode(connection, ticket, row)),
    ).optional()?;
    // A damaged or orphaned newest receipt cannot fall back to an older
    // acceptance or prevent unrelated operator edits from being committed.
    Ok(latest.and_then(Result::ok))
}

fn decode(connection: &Connection, ticket: i64, row: &Row<'_>) -> Result<DeliveryAcceptance> {
    let id: i64 = row.get(0)?;
    let assessment_id: i64 = row.get(1)?;
    let status = bounded_text(row, 2, 16)?;
    let digest = bounded_text(row, 3, 64)?;
    let created_at: f64 = row.get(4)?;
    ensure!(
        id > 0
            && assessment_id > 0
            && matches!(status.as_str(), "ready" | "partial")
            && valid_sha256(&digest)
            && created_at.is_finite()
            && created_at >= 0.,
        "操作员接受回执损坏"
    );
    let assessment = connection.query_row(
        "SELECT id,contract_revision,snapshot_id,policy_revision,manifest_sha256,status,created,unmet,reason FROM delivery_assessments WHERE id=? AND ticket=?",
        [assessment_id,ticket], |row| Ok(decode_assessment(row,ticket,assessment_id)),
    )??;
    let basis = behavior::saved_basis(connection, assessment_id)?;
    ensure!(
        status == assessment.status
            && digest.eq_ignore_ascii_case(&acceptance_digest(&assessment, &basis)?),
        "操作员接受回执与交付评估不符"
    );
    Ok(DeliveryAcceptance {
        id,
        ticket,
        assessment_id,
        snapshot_id: assessment.snapshot_id,
        contract_revision: assessment.contract_revision,
        policy_revision: assessment.policy_revision,
        status,
        assessment_sha256: digest,
        created_at,
    })
}
