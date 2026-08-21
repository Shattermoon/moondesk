#!/usr/bin/env node

const packageName = process.argv[2];
if (!packageName) {
  throw new Error("usage: npm-oidc-preflight.mjs <package-name>");
}

const requestUrl = process.env.ACTIONS_ID_TOKEN_REQUEST_URL;
const requestToken = process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
if (!requestUrl || !requestToken) {
  throw new Error("GitHub OIDC environment is unavailable; this job needs id-token: write");
}

const audience = "npm:registry.npmjs.org";
const oidcUrl = new URL(requestUrl);
oidcUrl.searchParams.append("audience", audience);

const oidcResponse = await fetch(oidcUrl, {
  headers: {
    Accept: "application/json",
    Authorization: `Bearer ${requestToken}`,
  },
});
if (!oidcResponse.ok) {
  throw new Error(`GitHub OIDC request failed with HTTP ${oidcResponse.status}`);
}

const oidcJson = await oidcResponse.json();
if (typeof oidcJson.value !== "string" || oidcJson.value.length === 0) {
  throw new Error("GitHub OIDC response did not contain an identity token");
}

const encodedPackage = encodeURIComponent(packageName);
const exchangeUrl = new URL(
  `/-/npm/v1/oidc/token/exchange/package/${encodedPackage}`,
  "https://registry.npmjs.org/",
);
const exchangeResponse = await fetch(exchangeUrl, {
  method: "POST",
  headers: {
    Authorization: `Bearer ${oidcJson.value}`,
  },
});

let exchangeJson = null;
try {
  exchangeJson = await exchangeResponse.json();
} catch {
  exchangeJson = null;
}

if (!exchangeResponse.ok) {
  const detail =
    exchangeJson?.message ??
    exchangeJson?.error ??
    "npm rejected the GitHub workflow identity";
  throw new Error(
    `npm Trusted Publishing token exchange failed with HTTP ${exchangeResponse.status}: ${detail}`,
  );
}

if (typeof exchangeJson?.token !== "string" || exchangeJson.token.length === 0) {
  throw new Error("npm Trusted Publishing token exchange returned no short-lived token");
}

console.log(`npm Trusted Publisher accepted this workflow identity for ${packageName}.`);
