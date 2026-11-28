/// Format integer paise (`Rs. 1 = 100 paise`) as `"₹12.50"` without floating-point arithmetic.
export function formatPaiseToInr(paise: number): string {
  const whole = Math.trunc(paise);
  const sign = whole < 0 ? "-" : "";
  const abs = Math.abs(whole);
  const rupees = Math.floor(abs / 100);
  const remPaise = abs % 100;
  return `${sign}\u20B9${rupees.toLocaleString("en-IN")}.${String(remPaise).padStart(2, "0")}`;
}

/// Parse a user-entered rupee string (up to 2 decimals) into integer paise without float math.
export function parseRupeesToPaise(raw: string): number | null {
  const trimmed = raw.trim();
  if (!/^\d+(?:\.\d{1,2})?$/.test(trimmed)) {
    return null;
  }
  const [rupeePart, paisePart = ""] = trimmed.split(".");
  const rupees = Number.parseInt(rupeePart, 10);
  const paise = Number.parseInt(paisePart.padEnd(2, "0"), 10);
  const total = rupees * 100 + paise;
  return Number.isSafeInteger(total) ? total : null;
}
