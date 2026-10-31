import { describe, expect, test } from "bun:test";
import { REDIRECTS, handle, latestDmgUrl, workerFirstGlobs } from "./worker.ts";

describe("worker-first rules", () => {
  test("send only the redirect paths, with and without a trailing slash", () => {
    expect(workerFirstGlobs(["/ios"])).toEqual(["/ios", "/ios/"]);
    expect(workerFirstGlobs()).toEqual(["/ios", "/ios/", "/download", "/download/"]);
  });

  test("leave every other path to the asset layer", () => {
    const rules = workerFirstGlobs();

    expect(rules).not.toContain("/*");
    for (const path of ["/", "/docs", "/blog/alpha", "/changelog"]) expect(rules).not.toContain(path);
  });
});

describe("redirects", () => {
  test("/ios goes to the TestFlight invite", async () => {
    for (const url of ["https://usepilus.dev/ios", "https://usepilus.dev/ios/", "https://usepilus.dev/ios?ref=x"]) {
      const response = await handle(new Request(url));

      expect(response.status).toBe(302);
      expect(response.headers.get("location")).toBe(REDIRECTS["/ios"]);
    }
  });

  test("do not catch paths that merely start the same way", async () => {
    expect((await handle(new Request("https://usepilus.dev/ios-beta"))).status).toBe(404);
    expect((await handle(new Request("https://usepilus.dev/constructor"))).status).toBe(404);
  });
});

const DMG = "https://releases.kepelet.com/pilus/pilus-v0.2.0-macos-arm64.dmg";
const manifest = (url: string) => ({
  version: "0.2.0",
  assets: [{ os: "macos", arch: "arm64", format: "dmg", url }],
});
const serving = (body: unknown, status = 200) => async () => Response.json(body, { status });

describe("/download", () => {
  test("redirects to the DMG the stable manifest names", async () => {
    for (const url of ["https://usepilus.dev/download", "https://usepilus.dev/download/"]) {
      const response = await handle(new Request(url), serving(manifest(DMG)));

      expect(response.status).toBe(302);
      expect(response.headers.get("location")).toBe(DMG);
    }
  });

  test("reads the stable manifest, not a hard-coded version", async () => {
    let asked = "";
    await latestDmgUrl(async (input) => {
      asked = input;
      return Response.json(manifest(DMG));
    });

    expect(asked).toBe("https://releases.kepelet.com/pilus/stable.json");
  });

  test("answers 502 when the manifest is down or malformed", async () => {
    const broken = [serving({}, 500), serving({ assets: [] }), serving("nope"), async () => { throw new Error("offline"); }];

    for (const fetcher of broken) expect((await handle(new Request("https://usepilus.dev/download"), fetcher)).status).toBe(502);
  });

  test("never redirects outside the releases host", async () => {
    const response = await handle(new Request("https://usepilus.dev/download"), serving(manifest("https://evil.example/pilus.dmg")));

    expect(response.status).toBe(502);
  });
});
