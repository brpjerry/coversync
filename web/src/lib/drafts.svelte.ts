// Unsaved edits that live in a component rather than a store, so that
// leaving (signing out) can ask first. The setup screens' edits are in their
// stores (`changes`).

export const drafts = $state({
	/** The tag editor has unsaved changes. */
	tags: false
});
