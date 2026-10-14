import assert from 'node:assert/strict';
import test from 'node:test';
import { isVersionOnlyReleaseCommit, releaseBatchFingerprint, releaseCommitShas } from './release-prs.mjs';
import { renderReleaseNotesBatch } from './pr-changelog.mjs';

const sha1 = 'a'.repeat(40);
const sha2 = 'b'.repeat(40);
const pr = (number, text) => ({
  number, title: `fix: PR ${number}`, body: `## Changelog\n\n- ${text}`,
  html_url: `https://github.com/Shattermoon/moondesk/pull/${number}`,
  user: { login: 'contributor' },
});

test('first-parent traversal captures every unreleased merge/squash commit in order', () => {
  assert.deepEqual(releaseCommitShas('v0.15.0', sha2, (...args) => {
    assert.deepEqual(args, ['rev-list', '--first-parent', '--reverse', `v0.15.0..${sha2}`]);
    return `${sha1}\n${sha2}`;
  }), [sha1, sha2]);
  assert.deepEqual(releaseCommitShas('', sha2), [sha2]);
});

test('release notes aggregate multiple PRs including skipped earlier releases', () => {
  const notes = renderReleaseNotesBatch([pr(1, 'Old fix.'), pr(2, 'New fix.')], {
    tag: 'v0.15.1', previousTag: 'v0.15.0', repository: 'Shattermoon/moondesk',
  });
  assert.match(notes, /- Old fix\.[\s\S]*- New fix\./);
  assert.match(notes, /#1 fix: PR 1/);
  assert.match(notes, /#2 fix: PR 2/);
});

test('internal-only PRs do not suppress user-facing notes from the same release', () => {
  const notes = renderReleaseNotesBatch([
    { ...pr(1, 'unused'), body: '## Changelog\n\nNo user-visible changelog.' },
    pr(2, 'A visible fix.'),
  ], { tag: 'v0.15.1', repository: 'Shattermoon/moondesk' });
  assert.ok(notes.includes('- A visible fix.'));
  assert.ok(!notes.includes('No user-visible changelog.'));
});

test('metadata fingerprint tracks all PRs and rejects duplicate PRs', () => {
  const original = { baseSha: sha2, previousTag: 'v0.15.0', prs: [pr(1, 'Old.'), pr(2, 'New.')] };
  assert.notEqual(releaseBatchFingerprint(original), releaseBatchFingerprint({ ...original, prs: [pr(1, 'Changed.'), pr(2, 'New.')] }));
  assert.notEqual(releaseBatchFingerprint(original), releaseBatchFingerprint({ ...original, previousTag: 'v0.14.9' }));
  assert.throws(() => releaseBatchFingerprint({ ...original, prs: [pr(1, 'Old.'), pr(1, 'Again.')] }), /Duplicate/);
});

test('only generated version-only release commits may be skipped', () => {
  const check = (author, email, subject, files) => isVersionOnlyReleaseCommit(sha1, (...args) =>
    args[0] === 'show' ? `${author}\n${email}\n${subject}` : files.join('\n'));
  const bot = 'github-actions[bot]';
  const mail = '41898282+github-actions[bot]@users.noreply.github.com';
  const files = ['Cargo.lock', 'Cargo.toml', 'package.json'];
  assert.equal(check(bot, mail, 'chore(release): v0.15.0', files), true);
  assert.equal(check('other', mail, 'chore(release): v0.15.0', files), false);
  assert.equal(check(bot, mail, 'chore(release): v0.15.0', [...files, 'src/main.rs']), false);
  assert.equal(check(bot, mail, 'fix: behavior', files), false);
});
