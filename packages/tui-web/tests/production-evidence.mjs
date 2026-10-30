// Observes the actual retained destination, never a scratch Canvas or fake UI.
export async function committedCheckpoint(label, canvas, regions = [], activation = null) {
  if (!(canvas instanceof HTMLCanvasElement) || !canvas.width || !canvas.height) {
    throw new Error(`${label}: no committed Canvas`);
  }
  const pixels = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data;
  const first = pixels.slice(0, 4);
  let ink = 0;
  for (let i = 0; i < pixels.length; i += 4) {
    if (pixels.subarray(i, i + 4).some((value, channel) => value !== first[channel])) ink++;
  }
  if (!ink) throw new Error(`${label}: destination contains no visible ink`);
  const digest = [...new Uint8Array(await crypto.subtle.digest('SHA-256', pixels))]
    .map(value => value.toString(16).padStart(2, '0')).join('');
  const checkpoint = {label, width: canvas.width, height: canvas.height, ink,
    pixel_sha256: digest, regions: structuredClone(regions)};
  if (activation) {
    const region = regions.find(region => (activation.id ? region.id === activation.id : region.label === activation.label) && !region.disabled);
    if (!region?.area?.width || !region.area.height) throw new Error(`${label}: missing enabled committed target; available=${regions.map(region => region.id).join(',')}`);
    const rect = canvas.getBoundingClientRect();
    const x = (region.area.x + region.area.width / 2) * activation.cellWidth;
    const y = (region.area.y + region.area.height / 2) * activation.cellHeight;
    if (x < 0 || x >= canvas.width || y < 0 || y >= canvas.height) throw new Error(`${label}: target outside committed Canvas`);
    checkpoint.click = {x: rect.left + x * rect.width / canvas.width,
      y: rect.top + y * rect.height / canvas.height, region_id: region.id};
  }
  globalThis.__bcodeAcceptanceCheckpoint = checkpoint;
  const deadline = performance.now() + 60000;
  while (globalThis.__bcodeAcceptanceAcknowledged !== label) {
    if (performance.now() > deadline) throw new Error(`${label}: screenshot observer unavailable`);
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  return checkpoint;
}
