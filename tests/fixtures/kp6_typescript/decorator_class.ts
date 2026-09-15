// KP-6-typescript curated snippet: decorator-like-annotation adjacency.
//
// Two decorated exported classes. The decorator and the class are named
// siblings inside the export_statement; a comment between them is legal
// typescript (decorators stack above the declaration), so strict sees
// only the clean pair, skip sees both.

@Component({ selector: "clean" })
export class CleanPanel {}

@Component({ selector: "split" })
// registered late; keep adjacent to its decorator
export class LatePanel {}
