const severityRank = { info: 0, ticket: 1, page: 2 };

const strongest = (left, right) =>
  (severityRank[right] ?? -1) > (severityRank[left] ?? -1) ? right : left;

/**
 * Correlate raw signals into operator-facing incidents.
 *
 * The seeded key contains one production defect: it includes the unique signal
 * id, so evidence from the same deployment cannot join the same incident.
 */
export function correlateSignals(signals, windowMinutes = 10) {
  const windowMs = windowMinutes * 60 * 1000;
  const incidents = [];

  for (const signal of signals) {
    const observedAt = Date.parse(signal.observed_at);
    const correlationKey = `${signal.service}:${signal.deployment}:${signal.id}`;
    const existing = incidents.find((incident) =>
      incident.correlation_key === correlationKey
      && observedAt - incident.first_observed_ms <= windowMs,
    );

    if (existing) {
      existing.signal_ids.push(signal.id);
      existing.severity = strongest(existing.severity, signal.severity);
      existing.last_observed_ms = observedAt;
      continue;
    }

    incidents.push({
      id: `inc-${incidents.length + 1}`,
      correlation_key: correlationKey,
      service: signal.service,
      deployment: signal.deployment,
      severity: signal.severity,
      signal_ids: [signal.id],
      first_observed_ms: observedAt,
      last_observed_ms: observedAt,
    });
  }

  return incidents.map(({ first_observed_ms, last_observed_ms, ...incident }) => ({
    ...incident,
    first_observed_at: new Date(first_observed_ms).toISOString(),
    last_observed_at: new Date(last_observed_ms).toISOString(),
  }));
}
