// OmniMCP Code Mode v6.8.5 — Canonical Patterns
// Always invoke tools via `codemode.<service>.<tool>(args)`.

// Pattern 1: Parallel fan-out + in-sandbox projection
const parallelReadPattern = async () => {
  const [driver, order] = await Promise.all([
    codemode.driver_dispatch.get_driver_location({ driver_id: "drv_101" }),
    codemode.accounting.get_ride_receipt({ ride_id: "ride_9001" }),
  ]);
  return {
    driver_id: driver.driver_id,
    driver_status: driver.status,
    total_cents: order.total_cents,
  };
};

// Pattern 2: Collection filtering & aggregation inside the sandbox
const filterAndSummarizePattern = async () => {
  const res = await codemode.driver_dispatch.list_active_drivers({ region: "us-west" });
  const available = res.drivers.filter((d: any) => d.status === "AVAILABLE");
  return {
    total: res.drivers.length,
    available_count: available.length,
    top_ids: available.slice(0, 5).map((d: any) => d.driver_id),
  };
};

// Pattern 3: Governed mutation (run with require_mutation_approval: true)
const governedMutationPattern = async () => {
  const receipt = await codemode.accounting.get_ride_receipt({ ride_id: "ride_9001" });
  if (receipt.total_cents <= 0) {
    return { skipped: true, reason: "zero_amount" };
  }
  const refund = await codemode.accounting.refund({
    ride_id: "ride_9001",
    reason: "duplicate_charge",
  });
  return { refunded: true, refund_id: refund.id };
};

export { parallelReadPattern, filterAndSummarizePattern, governedMutationPattern };
