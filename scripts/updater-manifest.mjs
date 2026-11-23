#!/usr/bin/env node
// 从 release-assets 的实际文件名生成 latest.json：URL 与 Release 上的资产名必须逐字一致，
// 手写名字迟早会和打包产物漂移
import { readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";

const REPO = "whispering233/agent-config-sync";

// 平台键由安装包格式决定；后缀同时用来在 release-assets 里认领对应文件
const PLATFORMS = [
  { key: "linux-x86_64-appimage", suffix: ".AppImage" },
  { key: "linux-x86_64-deb", suffix: ".deb" },
  { key: "windows-x86_64-nsis", suffix: "-setup.exe" },
];

const { values } = parseArgs({
  options: {
    dir: { type: "string", default: "release-assets" },
    tag: { type: "string" },
    out: { type: "string", default: "latest.json" },
  },
});

if (!values.tag || !/^v\d/.test(values.tag)) {
  throw new Error("--tag 必填，形如 v1.2.3");
}
const tag = values.tag;
const version = tag.replace(/^v/, "");

const files = readdirSync(values.dir).filter((name) =>
  statSync(join(values.dir, name)).isFile(),
);
// 空格会被 GitHub 归一化成 %20，客户端按 latest.json 里的名字取资产会对不上，直接拒绝
const spaced = files.filter((name) => /\s/.test(name));
if (spaced.length > 0) {
  throw new Error(`资产名不能含空格：${spaced.join(", ")}`);
}

const readSignature = (asset) => {
  const sig = `${asset}.sig`;
  if (!files.includes(sig)) {
    throw new Error(`缺少 ${sig}：没有签名私钥就不会产出 .sig，发布不能继续`);
  }
  const signature = readFileSync(join(values.dir, sig), "utf8").trim();
  if (signature === "") {
    throw new Error(`${sig} 为空`);
  }
  return signature;
};

const platforms = {};
for (const { key, suffix } of PLATFORMS) {
  const matches = files.filter(
    (name) => name.startsWith(`agent-config-sync-${version}-`) && name.endsWith(suffix),
  );
  if (matches.length !== 1) {
    throw new Error(`${key} 需要恰好一个 *${suffix} 资产，实际 ${matches.length} 个：${matches.join(", ") || "无"}`);
  }
  const asset = matches[0];
  platforms[key] = {
    signature: readSignature(asset),
    url: `https://github.com/${REPO}/releases/download/${tag}/${asset}`,
  };
}

const manifest = {
  version,
  pub_date: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
  platforms,
};
writeFileSync(values.out, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`${values.out}：${version}，${Object.keys(platforms).length} 个平台`);