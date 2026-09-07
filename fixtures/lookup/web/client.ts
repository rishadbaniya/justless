export function parseHttpHeader(line: string): [string, string] | undefined {
  const index = line.indexOf(':');
  if (index < 0) {
    return undefined;
  }
  return [line.slice(0, index).trim(), line.slice(index + 1).trim()];
}

export class DownloadQueue {
  private pending: string[] = [];

  retry(url: string): void {
    this.pending.push(url);
  }
}
