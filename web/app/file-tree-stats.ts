export interface ChangeStats {
  additions: number;
  deletions: number;
}

interface ChangedFile {
  name: string;
  hunks: readonly {
    additionLines: number;
    deletionLines: number;
  }[];
}

export function fileTreeChangeStats(files: readonly ChangedFile[]) {
  const statsByPath = new Map<string, ChangeStats>();

  for (const file of files) {
    const stats = fileChangeStats(file);
    addStats(statsByPath, file.name, stats);

    const segments = file.name.split("/");
    for (let end = 1; end < segments.length; end += 1) {
      addStats(statsByPath, `${segments.slice(0, end).join("/")}/`, stats);
    }
  }

  return statsByPath;
}

export function changeStats(files: readonly ChangedFile[]) {
  const total = { additions: 0, deletions: 0 };
  for (const file of files) {
    const stats = fileChangeStats(file);
    total.additions += stats.additions;
    total.deletions += stats.deletions;
  }
  return total;
}

function fileChangeStats(file: ChangedFile) {
  const stats = { additions: 0, deletions: 0 };
  for (const hunk of file.hunks) {
    stats.additions += hunk.additionLines;
    stats.deletions += hunk.deletionLines;
  }
  return stats;
}

function addStats(
  statsByPath: Map<string, ChangeStats>,
  path: string,
  added: ChangeStats,
) {
  const stats = statsByPath.get(path) ?? { additions: 0, deletions: 0 };
  stats.additions += added.additions;
  stats.deletions += added.deletions;
  statsByPath.set(path, stats);
}
