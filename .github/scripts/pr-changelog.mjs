#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export const NO_USER_VISIBLE_CHANGELOG = "No user-visible changelog.";
export const CHANGELOG_PLACEHOLDER = "- Describe the user-visible change here.";

function normalizeMarkdown(value) {
  return String(value ?? "").replace(/\r\n?/g, "\n");
}

function trimBlankLines(lines) {
  let start = 0;
  let end = lines.length;
  while (start < end && lines[start].trim() === "") start += 1;
  while (end > start && lines[end - 1].trim() === "") end -= 1;
  return lines.slice(start, end).join("\n");
}

function markdownLineRecords(lines) {
  const records = [];
  let fence = null;
  let inHtmlComment = false;

  for (const line of lines) {
    if (fence) {
      records.push({ visible: "", rendered: line });
      const trimmed = line.trim();
      const marker = fence.character.repeat(fence.length);
      if (trimmed.startsWith(marker) && /^(`{3,}|~{3,})\s*$/.test(trimmed)) {
        fence = null;
      }
      continue;
    }

    let visible = "";
    let cursor = 0;
    while (cursor < line.length) {
      if (inHtmlComment) {
        const commentEnd = line.indexOf("-->", cursor);
        if (commentEnd < 0) {
          cursor = line.length;
          break;
        }
        inHtmlComment = false;
        cursor = commentEnd + 3;
        continue;
      }

      const commentStart = line.indexOf("<!--", cursor);
      if (commentStart < 0) {
        visible += line.slice(cursor);
        break;
      }
      visible += line.slice(cursor, commentStart);
      const commentEnd = line.indexOf("-->", commentStart + 4);
      if (commentEnd < 0) {
        inHtmlComment = true;
        cursor = line.length;
        break;
      }
      cursor = commentEnd + 3;
    }

    visible = visible.replace(/[ \t]+$/, "");
    const fenceMatch = /^ {0,3}(`{3,}|~{3,})/.exec(visible);
    if (fenceMatch) {
      const token = fenceMatch[1];
      fence = { character: token[0], length: token.length };
      records.push({ visible: "", rendered: visible });
      continue;
    }

    records.push({ visible, rendered: visible });
  }

  return records;
}

export function extractChangelog(body) {
  const lines = normalizeMarkdown(body).split("\n");
  const records = markdownLineRecords(lines);
  const headings = [];

  for (let index = 0; index < lines.length; index += 1) {
    if (/^ {0,3}##[ \t]+Changelog[ \t]*$/.test(records[index].visible)) {
      headings.push(index);
    }
  }

  if (headings.length === 0) {
    throw new Error("PR description must contain exactly one `## Changelog` section.");
  }
  if (headings.length !== 1) {
    throw new Error("PR description contains multiple `## Changelog` sections.");
  }

  const start = headings[0] + 1;
  let end = lines.length;
  for (let index = start; index < lines.length; index += 1) {
    if (/^ {0,3}#{1,2}(?:[ \t]+\S|[ \t]*$)/.test(records[index].visible)) {
      end = index;
      break;
    }
  }

  const changelog = trimBlankLines(
    records.slice(start, end).map((record) => record.rendered),
  );
  if (!changelog) {
    throw new Error("`## Changelog` must not be empty.");
  }
  return changelog;
}

export function validateChangelog(body) {
  const changelog = extractChangelog(body);
  if (changelog.split("\n").some((line) => line.trim() === CHANGELOG_PLACEHOLDER)) {
    throw new Error("Replace the PR template changelog placeholder with real release notes.");
  }
  if (changelog === NO_USER_VISIBLE_CHANGELOG) {
    return changelog;
  }

  const lines = changelog.split("\n");
  const records = markdownLineRecords(lines);
  const hasBullet = records.some((record) => /^[-*+][ \t]+\S/.test(record.visible));
  if (!hasBullet) {
    throw new Error(
      "`## Changelog` must contain at least one Markdown bullet or exactly " +
        `${JSON.stringify(NO_USER_VISIBLE_CHANGELOG)}.`,
    );
  }

  return changelog;
}

export function releaseMetadataFingerprint(pr) {
  if (!pr || typeof pr !== "object") {
    throw new Error("Pull request JSON is required.");
  }

  const canonical = JSON.stringify({
    number: Number(pr.number),
    title: String(pr.title ?? ""),
    changelog: validateChangelog(pr.body ?? ""),
    html_url: String(pr.html_url ?? ""),
    user_login: String(pr.user?.login ?? ""),
  });
  return createHash("sha256").update(canonical, "utf8").digest("hex");
}

export function renderReleaseNotes(pr, options) {
  if (!pr || typeof pr !== "object") {
    throw new Error("Pull request JSON is required.");
  }
  const changelog = validateChangelog(pr.body ?? "");
  const number = Number(pr.number);
  if (!Number.isInteger(number) || number <= 0) {
    throw new Error("Pull request number is missing or invalid.");
  }

  const title = String(pr.title ?? "").trim();
  if (!title) throw new Error("Pull request title is missing.");

  const repository = String(options.repository ?? "").trim();
  const tag = String(options.tag ?? "").trim();
  if (!/^[^/\s]+\/[^/\s]+$/.test(repository)) {
    throw new Error("Repository must use owner/name form.");
  }
  if (!/^v\d+\.\d+\.\d+$/.test(tag)) {
    throw new Error("Release tag must use vX.Y.Z form.");
  }

  const prUrl =
    typeof pr.html_url === "string" && pr.html_url.startsWith("https://")
      ? pr.html_url
      : `https://github.com/${repository}/pull/${number}`;
  const author =
    typeof pr.user?.login === "string" && pr.user.login.trim()
      ? ` by @${pr.user.login.trim()}`
      : "";
  const linkTitle = title.replace(/\\/g, "\\\\").replace(/\[/g, "\\[").replace(/\]/g, "\\]");

  const lines = [
    "## Changelog",
    "",
    changelog,
    "",
    "## Pull request",
    "",
    `- [#${number} ${linkTitle}](${prUrl})${author}`,
  ];

  const previousTag = String(options.previousTag ?? "").trim();
  if (previousTag) {
    if (!/^v\d+\.\d+\.\d+$/.test(previousTag)) {
      throw new Error("Previous release tag must use vX.Y.Z form.");
    }
    lines.push(
      "",
      `**Full Changelog**: https://github.com/${repository}/compare/${previousTag}...${tag}`,
    );
  }

  return `${lines.join("\n").trim()}\n`;
}

function loadJson(path, label) {
  if (!path) throw new Error(`${label} path is required.`);
  return JSON.parse(readFileSync(path, "utf8"));
}

function usage() {
  return [
    "Usage:",
    "  node .github/scripts/pr-changelog.mjs validate-event <github-event.json>",
    "  node .github/scripts/pr-changelog.mjs validate-pr <pull-request.json>",
    "  node .github/scripts/pr-changelog.mjs fingerprint-pr <pull-request.json>",
    "  node .github/scripts/pr-changelog.mjs render-release <pull-request.json> <tag> <previous-tag-or-dash> <owner/repo> <output.md>",
  ].join("\n");
}

function main(argv) {
  const [command, ...args] = argv;

  if (command === "validate-event") {
    const event = loadJson(args[0] ?? process.env.GITHUB_EVENT_PATH, "GitHub event");
    if (!event.pull_request) throw new Error("GitHub event does not contain pull_request data.");
    const changelog = validateChangelog(event.pull_request.body ?? "");
    process.stdout.write(`Validated PR changelog: ${changelog === NO_USER_VISIBLE_CHANGELOG ? "internal-only" : "user-visible"}\n`);
    return;
  }

  if (command === "validate-pr") {
    const pr = loadJson(args[0], "Pull request JSON");
    const changelog = validateChangelog(pr.body ?? "");
    process.stdout.write(`Validated PR #${pr.number ?? "?"} changelog: ${changelog === NO_USER_VISIBLE_CHANGELOG ? "internal-only" : "user-visible"}\n`);
    return;
  }

  if (command === "fingerprint-pr") {
    const pr = loadJson(args[0], "Pull request JSON");
    process.stdout.write(`${releaseMetadataFingerprint(pr)}\n`);
    return;
  }

  if (command === "render-release") {
    const [prPath, tag, previousTagRaw, repository, outputPath] = args;
    if (!outputPath) throw new Error("Release notes output path is required.");
    const pr = loadJson(prPath, "Pull request JSON");
    const notes = renderReleaseNotes(pr, {
      tag,
      previousTag: previousTagRaw === "-" ? "" : previousTagRaw,
      repository,
    });
    writeFileSync(outputPath, notes, "utf8");
    process.stdout.write(`Rendered release notes for ${tag} from PR #${pr.number}.\n`);
    return;
  }

  throw new Error(usage());
}

const invokedPath = process.argv[1] ? pathToFileURL(process.argv[1]).href : "";
if (invokedPath === import.meta.url) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
