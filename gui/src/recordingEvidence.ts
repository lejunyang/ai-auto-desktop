/** Screenshots are opened only from the directory selected by the user. */
export async function readEvidence(files: File[]) {
  const rows = files.find((f) => /^[^/]+\/steps\.jsonl$/.test(f.webkitRelativePath));
  if (!rows || rows.size > 16 * 1024 * 1024) throw new Error("请选择包含 steps.jsonl 的浏览器录制目录（步骤文件上限 16 MiB）");
  const root = rows.webkitRelativePath.slice(0, -"steps.jsonl".length);
  const images = new Map(files.map((f) => [f.webkitRelativePath, f]));
  const screenshots = new Map<number, File[]>();
  const text = await rows.text();
  for (const line of text.split("\n").filter((value) => value.trim())) {
    const row = JSON.parse(line);
    if (!Number.isSafeInteger(row.id) || screenshots.has(row.id)) throw new Error("录制步骤编号缺失或重复");
    const shots: File[] = [];
    for (const shot of row.screenshots ?? []) {
      if (!/^images\/[a-f0-9]{64}\.png$/.test(shot.path)) continue;
      const file = images.get(root + shot.path);
      if (file && file.size <= 16 * 1024 * 1024) shots.push(file);
    }
    screenshots.set(row.id, shots);
  }
  return { text, screenshots, name: root.slice(0, -1) };
}
