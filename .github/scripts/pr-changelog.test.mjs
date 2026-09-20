import assert from "node:assert/strict";
import test from "node:test";

import {
  CHANGELOG_PLACEHOLDER,
  NO_USER_VISIBLE_CHANGELOG,
  extractChangelog,
  releaseMetadataFingerprint,
  renderReleaseNotes,
  validateChangelog,
} from "./pr-changelog.mjs";

test("extracts one changelog section and stops at the next level-two heading", () => {
  const body = `## Summary

Details.

## Changelog

- Fixed startup.
- Improved recovery.

## Validation

- Tests passed.
`;
  assert.equal(
    extractChangelog(body),
    "- Fixed startup.\n- Improved recovery.",
  );
});

test("ignores changelog-looking headings inside fenced and indented code blocks", () => {
  const body = `## Summary

\`\`\`md
## Changelog
- fake fenced
\`\`\`

    ## Changelog
    - fake indented

## Changelog

- Real user-visible change.
`;
  assert.equal(validateChangelog(body), "- Real user-visible change.");
});

test("requires exactly one non-empty changelog section", () => {
  assert.throws(() => validateChangelog("## Summary\nNothing"), /exactly one/);
  assert.throws(() => validateChangelog("## Changelog\n\n## Validation\n- ok"), /must not be empty/);
  assert.throws(
    () => validateChangelog("## Changelog\n- one\n\n## Changelog\n- two"),
    /multiple/,
  );
});

test("requires release-note bullets or the explicit internal-only sentence", () => {
  assert.throws(
    () => validateChangelog(`## Changelog\n\n${CHANGELOG_PLACEHOLDER}`),
    /Replace the PR template changelog placeholder/,
  );
  assert.throws(
    () => validateChangelog("## Changelog\nFixed a thing."),
    /Markdown bullet/,
  );
  assert.equal(
    validateChangelog(`## Changelog\n${NO_USER_VISIBLE_CHANGELOG}`),
    NO_USER_VISIBLE_CHANGELOG,
  );
});

test("renders release notes from the PR changelog with PR and comparison links", () => {
  const notes = renderReleaseNotes(
    {
      number: 63,
      title: "fix: improve [release] changelog",
      body: "## Changelog\n\n- Release notes now show user-facing changes.",
      html_url: "https://github.com/Shattermoon/moondesk/pull/63",
      user: { login: "nkcbuilds" },
    },
    {
      tag: "v0.13.4",
      previousTag: "v0.13.3",
      repository: "Shattermoon/moondesk",
    },
  );

  assert.match(notes, /^## Changelog/m);
  assert.match(notes, /Release notes now show user-facing changes/);
  assert.ok(notes.includes("[#63 fix: improve \\[release\\] changelog]"));
  assert.match(notes, /by @nkcbuilds/);
  assert.match(notes, /compare\/v0\.13\.3\.\.\.v0\.13\.4/);
});

test("release metadata fingerprint is stable across line endings and changes with release-facing PR metadata", () => {
  const base = {
    number: 63,
    title: "fix: enforce release changelog",
    body: "## Changelog\r\n\r\n- Better notes.\r\n",
    html_url: "https://github.com/Shattermoon/moondesk/pull/63",
    user: { login: "nkcbuilds" },
  };
  const normalized = {
    ...base,
    body: "## Changelog\n\n- Better notes.\n",
  };
  assert.equal(releaseMetadataFingerprint(base), releaseMetadataFingerprint(normalized));
  assert.notEqual(
    releaseMetadataFingerprint(base),
    releaseMetadataFingerprint({ ...normalized, title: "fix: changed after merge" }),
  );
  assert.notEqual(
    releaseMetadataFingerprint(base),
    releaseMetadataFingerprint({ ...normalized, body: "## Changelog\n\n- Different notes.\n" }),
  );
});

test("release rendering validates tag and repository inputs", () => {
  const pr = {
    number: 1,
    title: "fix: example",
    body: "## Changelog\n- Example.",
  };
  assert.throws(
    () => renderReleaseNotes(pr, { tag: "0.1.0", repository: "owner/repo" }),
    /vX.Y.Z/,
  );
  assert.throws(
    () => renderReleaseNotes(pr, { tag: "v0.1.0", repository: "invalid" }),
    /owner\/name/,
  );
});
