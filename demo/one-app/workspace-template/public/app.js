const orders = document.querySelector("#orders");
const summary = document.querySelector("#summary");
const refresh = document.querySelector("#refresh");

const money = new Intl.NumberFormat("en-US", {
  style: "currency",
  currency: "USD",
});

async function render() {
  refresh.disabled = true;
  try {
    const response = await fetch(`api/orders?at=${Date.now()}`);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const data = await response.json();
    const invalid = data.filter((order) => order.payable < 0);
    orders.innerHTML = data.map((order) => `
      <article class="${order.payable < 0 ? "invalid" : "valid"}">
        <div>
          <p class="order-id">${order.id}</p>
          <h2>${order.label}</h2>
          <p>${order.customer}</p>
        </div>
        <div class="amount">
          <span>Payable</span>
          <strong>${money.format(order.payable)}</strong>
          <small>${order.payable < 0 ? "Invariant broken" : "Ready"}</small>
        </div>
      </article>
    `).join("");
    summary.textContent = invalid.length
      ? `${invalid.length} order violates the payable-total invariant`
      : "All payable totals satisfy the invariant";
    summary.className = invalid.length ? "summary invalid" : "summary valid";
  } catch (error) {
    orders.innerHTML = `<p class="error">Could not load the preview: ${error.message}</p>`;
    summary.textContent = "Preview unavailable";
  } finally {
    refresh.disabled = false;
  }
}

refresh.addEventListener("click", render);
render();
