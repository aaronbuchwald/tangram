// Apps' UIs (apps/<app>/ui/index.html) are bundled as text via the Text
// rule in wrangler.toml.
declare module "*.html" {
  const text: string;
  export default text;
}
