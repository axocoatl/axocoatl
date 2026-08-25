const summary = document.querySelector("#summary");
const incidents = document.querySelector("#incidents");
const refresh = document.querySelector("#refresh");

async function render() {
  refresh.disabled = true;
  try {
    const response = await fetch(`api/incidents?at=${Date.now()}`);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const rows = await response.json();
    summary.className = rows.length === 1 ? "good" : "bad";
    summary.textContent = rows.length === 1
      ? "1 incident · one page · all evidence retained"
      : `${rows.length} incidents · duplicate pages for one deployment`;
    incidents.innerHTML = rows.map((incident) => `
      <article>
        <div><span>${incident.severity}</span><strong>${incident.id}</strong></div>
        <p>${incident.signal_ids.join(" → ")}</p>
      </article>
    `).join("");
  } catch (error) {
    summary.className = "bad";
    summary.textContent = `Preview unavailable: ${error.message}`;
    incidents.innerHTML = "";
  } finally {
    refresh.disabled = false;
  }
}

refresh.addEventListener("click", render);
render();
