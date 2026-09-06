import {test, expect} from 'bun:test';
import {readFileSync} from 'node:fs';
import {resolve} from 'node:path';
require('release-please'); // Initialize the CommonJS registry before its strategies.
const {Rust} = require('release-please/build/src/strategies/rust');
import {Version} from 'release-please/build/src/version';
import {TagName} from 'release-please/build/src/util/tag-name';
import {parseConventionalCommits} from 'release-please/build/src/commit';

const root = resolve(import.meta.dir, '../..');
const config = JSON.parse(readFileSync(resolve(root, 'release-please-config.json')));
const current = Bun.TOML.parse(readFileSync(resolve(root, 'Cargo.toml'), 'utf8')).package.version;
const logger = {info() {}, warn() {}, debug() {}, error() {}};

for (const [message, path, bump] of [
  ['feat(desktop): add a fixture capability', 'desktop/src/main.rs', 'minor'],
  ['fix(runtime): correct a fixture bug', 'src/daemon.rs', 'patch'],
]) {
  test(`${message}: one release updates both packages`, async () => {
    expect(Object.keys(config.packages)).toEqual(['.']);
    expect(config.packages['.']['release-type']).toBe('rust');
    const strategy = new Rust({
      github: {
        repository: {owner: 'gardnmi', repo: 'boomux'},
        getFileContentsOnBranch: async (path) => ({
          parsedContent: readFileSync(resolve(root, path), 'utf8'),
        }),
      },
      path: '.', targetBranch: 'main', packageName: 'boomux', logger,
      includeComponentInTag: config.packages['.']['include-component-in-tag'],
      changelogNotes: {buildNotes: async () => `## Changes\n\n* ${message}\n`},
    });
    const [major, minor, patch] = current.split('.').map(Number);
    const next = bump === 'minor' ? `${major}.${minor + 1}.0` : `${major}.${minor}.${patch + 1}`;
    const commits = parseConventionalCommits([{sha: 'a'.repeat(40), message, files: [path]}]);
    const proposal = await strategy.buildReleasePullRequest(commits, {
      tag: new TagName(Version.parse(current)), sha: 'b'.repeat(40), notes: '',
    });
    expect(proposal.version.toString()).toBe(next);
    const updated = new Map(proposal.updates.map(update => [update.path,
      update.updater.updateContent(readFileSync(resolve(root, update.path), 'utf8'), logger)]));
    for (const path of ['Cargo.toml', 'desktop/Cargo.toml']) {
      expect(Bun.TOML.parse(updated.get(path)).package.version).toBe(next);
    }
    const lock = Bun.TOML.parse(updated.get('Cargo.lock'));
    for (const name of ['boomux', 'boomux-desktop']) {
      expect(lock.package.filter(pkg => pkg.name === name && !pkg.source).map(pkg => pkg.version)).toEqual([next]);
    }
    const releases = await strategy.buildReleases({
      headBranchName: proposal.headRefName, baseBranchName: 'main', number: 999,
      mergeCommitOid: 'c'.repeat(40), sha: 'c'.repeat(40), title: proposal.title.toString(),
      body: proposal.body.toString(), labels: [], files: [...updated.keys()],
    });
    expect(releases).toHaveLength(1);
    expect(releases[0].tag.toString()).toBe(`v${next}`);
  });
}
