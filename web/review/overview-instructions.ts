export const MAX_OVERVIEW_INSTRUCTIONS_BYTES = 8 * 1024;

export function overviewInstructionError(value: string): string | undefined {
  const bytes = new TextEncoder().encode(value.trim()).length;
  if (bytes > MAX_OVERVIEW_INSTRUCTIONS_BYTES) {
    return `Instructions must be at most 8 KiB (currently ${bytes.toLocaleString()} bytes).`;
  }
}
