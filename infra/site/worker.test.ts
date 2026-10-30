import { describe, expect, test } from "bun:test";
import worker, { PUBLIC, workerFirstGlobs } from "./worker.ts";

describe("worker-first rules", () => {
  test("send everything to the Worker except the public paths", () => {
    expect(workerFirstGlobs(["/", "/_astro/*"])).toEqual(["/*", "!/", "!/_astro/*"]);
  });

  test("keep the landing page and what it loads on the asset layer", () => {
    const rules = workerFirstGlobs();

    for (const path of PUBLIC) expect(rules).toContain(`!${path}`);
    expect(rules[0]).toBe("/*");
  });

  test("do not expose a page", () => {
    for (const path of ["/docs", "/blog", "/changelog", "/download", "/sitemap.xml"]) {
      expect(PUBLIC).not.toContain(path);
      expect(PUBLIC.some((entry) => entry.endsWith("/*") && path.startsWith(entry.slice(0, -1)))).toBe(false);
    }
  });
});

describe("every other path", () => {
  test("answers 404 without naming the unpublished sections", async () => {
    for (const path of ["/docs", "/docs/typo?ref=x", "/blog", "/changelog", "/download", "/blog.md", "/nowhere"]) {
      const response = await worker.fetch();
      const body = await response.text();

      expect(response.status).toBe(404);
      expect(response.headers.get("content-type")).toBe("text/html; charset=utf-8");
      expect(body).toContain("noindex");
      for (const hidden of ["blog", "changelog", "download", "docs"]) {
        expect(body.toLowerCase()).not.toContain(hidden);
      }
    }
  });
});
