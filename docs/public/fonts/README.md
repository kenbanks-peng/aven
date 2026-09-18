# Fonts

Self-hosted latin subsets, served from `/fonts/` and declared in
`src/styles/fonts.css`.

| File                            | Family        | Weights |
| ------------------------------- | ------------- | ------- |
| `space-grotesk-latin.woff2`     | Space Grotesk | 400-700 |
| `ibm-plex-mono-latin.woff2`     | IBM Plex Mono | 400     |
| `ibm-plex-mono-latin-600.woff2` | IBM Plex Mono | 600     |

All files are the `latin` subset published by Google Fonts, downloaded from
`fonts.gstatic.com` via the `css2` API with a browser user agent. Every IBM Plex
Mono weight must come from the same family version so the weights cover
identical glyphs:

```sh
curl -s "https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@600&display=swap" \
  -H "User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"
```

Take the URL under the `/* latin */` comment and save it to this directory.

License texts for both families are alongside the font files.
