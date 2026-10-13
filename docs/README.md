# Broom site

Source of https://broom.hainth.edu.vn, built by GitHub Pages (classic Jekyll, whitelisted plugins only). Not published.

- When you change the visible content of a page, set its `last_modified_at:` to today. It feeds `<lastmod>` in
  `sitemap.xml` and `dateModified` in the page's JSON-LD.
- `date:` is the day the page was first added; leave it.
- The FAQ on `bootrom-quan-net.md` lives in its front matter (`faq:`); the page and its `FAQPage` JSON-LD are both
  rendered from it. Links there must point to `.html`.
- All pages share the Open Graph image `images/og-broom.jpg` (1200×630).
