import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { releaseMetadataFingerprint, renderReleaseNotesBatch } from './pr-changelog.mjs';

const git = (...args) => execFileSync('git', args, { encoding: 'utf8' }).trim();
const json = (path) => JSON.parse(readFileSync(path, 'utf8'));

export function releaseBatchFingerprint(batch) {
  if (!batch || !Array.isArray(batch.prs) || !batch.prs.length) throw new Error('Release PR list is empty.');
  const seen = new Set();
  const fingerprints = batch.prs.map((pr) => {
    if (!Number.isSafeInteger(pr.number) || pr.number < 1 || seen.has(pr.number)) throw new Error('Duplicate or invalid release PR.');
    seen.add(pr.number);
    return releaseMetadataFingerprint(pr);
  });
  return createHash('sha256').update(JSON.stringify({ baseSha: batch.baseSha, previousTag: batch.previousTag, fingerprints })).digest('hex');
}

export function releaseCommitShas(previousTag, baseSha, command = git) {
  if (!/^[0-9a-f]{40}$/.test(baseSha)) throw new Error('Invalid release source SHA.');
  if (previousTag && !/^v\d+\.\d+\.\d+$/.test(previousTag)) throw new Error('Invalid previous release tag.');
  if (!previousTag) return [baseSha];
  const output = command('rev-list', '--first-parent', '--reverse', `${previousTag}..${baseSha}`);
  const commits = output.split(/\s+/).filter(Boolean);
  if (!commits.length || commits.length > 200) throw new Error('Release source has no commits or too many commits to audit.');
  return commits;
}

export function isVersionOnlyReleaseCommit(sha, command = git) {
  const metadata = command('show', '-s', '--format=%an%n%ae%n%s', sha).split(/\r?\n/);
  if (metadata[0] !== 'github-actions[bot]' ||
      metadata[1] !== '41898282+github-actions[bot]@users.noreply.github.com' ||
      !/^chore\(release\): v\d+\.\d+\.\d+$/.test(metadata[2] ?? '')) return false;
  const files = command('diff-tree', '--no-commit-id', '--name-only', '-r', sha).split(/\r?\n/).filter(Boolean);
  const allowed = new Set(['package.json', 'Cargo.toml', 'Cargo.lock', 'extensions/moondesk-worker-companion/manifest.json']);
  return files.length >= 3 && files.every((file) => allowed.has(file));
}

function collect(baseSha, repository, outputPath) {
  if (!/^[\w.-]+\/[\w.-]+$/.test(repository)) throw new Error('Invalid GitHub repository.');
  const tags = git('tag', '--merged', baseSha, '--list', 'v[0-9]*', '--sort=-version:refname').split(/\r?\n/);
  const previousTag = tags.find((tag) => /^v\d+\.\d+\.\d+$/.test(tag)) ?? '';
  const prs = [];
  for (const commitSha of releaseCommitShas(previousTag, baseSha)) {
    if (isVersionOnlyReleaseCommit(commitSha)) continue;
    const linked = JSON.parse(execFileSync('gh', ['api', '-H', 'Accept: application/vnd.github+json', `repos/${repository}/commits/${commitSha}/pulls`], { encoding: 'utf8' }));
    const matches = linked.filter((pr) => pr.state === 'closed' && pr.merged_at && pr.base?.ref === 'main' && pr.merge_commit_sha === commitSha);
    if (matches.length !== 1) throw new Error(`Commit ${commitSha} must map to exactly one merged PR; found ${matches.length}.`);
    const pr = JSON.parse(execFileSync('gh', ['api', '-H', 'Accept: application/vnd.github+json', `repos/${repository}/pulls/${matches[0].number}`], { encoding: 'utf8' }));
    prs.push(pr);
  }
  const batch = { baseSha, previousTag, prs };
  releaseBatchFingerprint(batch);
  writeFileSync(outputPath, JSON.stringify(batch), 'utf8');
}

const [action, ...args] = process.argv.slice(2);
if (action === 'collect') collect(args[0], args[1], args[2]);
else if (action === 'fingerprint') process.stdout.write(`${releaseBatchFingerprint(json(args[0]))}\n`);
else if (action === 'render') {
  const batch = json(args[0]);
  const notes = renderReleaseNotesBatch(batch.prs, { tag: args[1], previousTag: batch.previousTag, repository: args[2] });
  writeFileSync(args[3], notes, 'utf8');
} else if (process.argv[1]?.endsWith('release-prs.mjs')) throw new Error('Invalid release-prs action.');
