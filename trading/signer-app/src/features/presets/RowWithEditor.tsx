// Shared table primitive: a row plus, when open, a full-width editor
// row attached right under it (spec §D — the editor sits where you
// clicked). Used by the HL copy-trade + callers tables.

import { type ReactNode } from "react";

export function RowWithEditor({
  children,
  editor,
  colSpan,
}: {
  children: ReactNode;
  editor: ReactNode | null;
  colSpan: number;
}) {
  return (
    <>
      <tr className={editor ? "row-editing" : undefined}>{children}</tr>
      {editor && (
        <tr className="row-editor">
          <td colSpan={colSpan}>{editor}</td>
        </tr>
      )}
    </>
  );
}
