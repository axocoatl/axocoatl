# Incident correlation policy

Signal Desk pages once for one operational incident, while retaining every
signal as evidence.

- Signals correlate when `service` and `deployment` match and their observation
  times fall within a rolling ten-minute window.
- A later signal more than ten minutes after the incident's first observation
  starts a new incident.
- The incident severity is the highest severity among its signals.
- Signal ids stay in observation order so an operator can reconstruct the route.
- Different services or deployments never correlate.
