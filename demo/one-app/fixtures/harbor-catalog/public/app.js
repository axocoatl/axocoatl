const proof = document.querySelector("#proof");
const rerun = document.querySelector("#rerun");

async function render() {
  rerun.disabled = true;
  try {
    const response = await fetch(`api/reproduction?at=${Date.now()}`);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const result = await response.json();
    proof.className = result.stale ? "bad" : "good";
    proof.innerHTML = `
      <span>${result.stale ? "Stale cache" : "Cache coherent"}</span>
      <strong>${result.after.length ? result.after[0].name : "No results"}</strong>
      <p>${result.stale
        ? "The product was added, but the cached search still returns nothing."
        : "The cached query reflects the catalog mutation."}</p>
    `;
  } catch (error) {
    proof.className = "bad";
    proof.textContent = `Preview unavailable: ${error.message}`;
  } finally {
    rerun.disabled = false;
  }
}

rerun.addEventListener("click", render);
render();
