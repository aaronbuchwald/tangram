// Derive a folder tree from the flat `Vec<MdFile>` (paths are `/`-separated).
// `.keep` sentinels (empty-folder markers, see the backend) materialize a
// folder but are not shown as files.

import type { MdFile } from "./api";

export interface FileNode {
  kind: "file";
  name: string;
  path: string;
  file: MdFile;
}

export interface FolderNode {
  kind: "folder";
  name: string;
  path: string;
  children: TreeNode[];
}

export type TreeNode = FolderNode | FileNode;

const KEEP = ".keep";

interface MutFolder {
  name: string;
  path: string;
  folders: Map<string, MutFolder>;
  files: FileNode[];
}

function emptyFolder(name: string, path: string): MutFolder {
  return { name, path, folders: new Map(), files: [] };
}

/** Build a sorted tree (folders first, then files, each alphabetical). */
export function buildTree(files: MdFile[]): TreeNode[] {
  const root = emptyFolder("", "");
  for (const file of files) {
    const segments = file.path.split("/").filter((s) => s.length > 0);
    if (segments.length === 0) continue;
    const filename = segments[segments.length - 1];
    let folder = root;
    let acc = "";
    for (let i = 0; i < segments.length - 1; i++) {
      const seg = segments[i];
      acc = acc ? `${acc}/${seg}` : seg;
      let next = folder.folders.get(seg);
      if (!next) {
        next = emptyFolder(seg, acc);
        folder.folders.set(seg, next);
      }
      folder = next;
    }
    // `.keep` only materializes the folder; it is never listed as a file.
    if (filename === KEEP) continue;
    folder.files.push({ kind: "file", name: filename, path: file.path, file });
  }
  return materialize(root);
}

function materialize(folder: MutFolder): TreeNode[] {
  const folders = [...folder.folders.values()]
    .sort((a, b) => a.name.localeCompare(b.name))
    .map<FolderNode>((f) => ({
      kind: "folder",
      name: f.name,
      path: f.path,
      children: materialize(f),
    }));
  const files = folder.files.sort((a, b) => a.name.localeCompare(b.name));
  return [...folders, ...files];
}
