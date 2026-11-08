/** esbuild inlines these with its `dataurl` loader, so the compiled main carries the mark. */
declare module '*.png' {
  const dataUrl: string
  export default dataUrl
}
