/** The one way this UI turns a thrown thing into a sentence: an Error's
 *  message, verbatim. A governance refusal IS that message, and paraphrasing it
 *  is how a console produces an error nobody can act on. */
export function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}
