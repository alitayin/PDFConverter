export type ConversionKind = 'pdf_to_doc' | 'pdf_to_docx' | 'pdf_to_odt' | 'pdf_to_odp' | 'pdf_to_ppt' | 'pdf_to_rtf' | 'pdf_to_flat_odt_xml' | 'doc_to_pdf' | 'docx_to_pdf' | 'odt_to_pdf' | 'txt_to_pdf' | 'rtf_to_pdf' | 'html_to_pdf' | 'markdown_to_pdf' | 'pdf_to_image' | 'pdf_to_txt' | 'pdf_to_markdown' | 'pdf_to_pptx' | 'image_to_pdf' | 'svg_to_pdf' | 'ppt_to_pdf' | 'pptx_to_pdf' | 'odp_to_pdf' | 'xlsx_to_pdf' | 'ods_to_pdf';

export type ConversionGroup = 'pdf' | 'word' | 'slides' | 'spreadsheets' | 'images';

export const conversionGroups: Array<{ id: ConversionGroup; title: string }> = [
  { id: 'pdf', title: 'PDF' },
  { id: 'word', title: 'Documents' },
  { id: 'slides', title: 'Presentations' },
  { id: 'spreadsheets', title: 'Spreadsheets' },
  { id: 'images', title: 'Images' }
];

export type JobState = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'timed_out';

export type ConversionOptions = {
  imageFormat?: 'png' | 'jpg' | 'bmp' | 'gif' | 'webp' | 'tiff';
  dpi?: 150 | 200 | 300;
  pages?: string;
  jpgQuality?: number;
  /** Merge selected static images into one ordered multi-page PDF. */
  mergeImages?: boolean;
};

export type ConversionFile = {
  id: string;
  file: File;
  size: number;
  path?: string;
  state: JobState;
  progress: number;
  message?: string;
  outputName?: string;
  outputs?: string[];
};

export const conversionModes: Array<{
  kind: ConversionKind;
  group: ConversionGroup;
  title: string;
  description: string;
  input: string;
  output: string;
}> = [
  { kind: 'pdf_to_docx', group: 'pdf', title: 'PDF to DOCX', description: 'Editable DOCX output', input: 'PDF', output: 'DOCX' },
  { kind: 'pdf_to_doc', group: 'pdf', title: 'PDF to DOC', description: 'Legacy Word output', input: 'PDF', output: 'DOC' },
  { kind: 'pdf_to_odt', group: 'pdf', title: 'PDF to ODT', description: 'OpenDocument output', input: 'PDF', output: 'ODT' },
  { kind: 'pdf_to_image', group: 'pdf', title: 'PDF to Images', description: 'Export each page as an image', input: 'PDF', output: 'Images' },
  { kind: 'pdf_to_txt', group: 'pdf', title: 'PDF to TXT', description: 'Extract the existing text layer', input: 'PDF', output: 'TXT' },
  { kind: 'pdf_to_markdown', group: 'pdf', title: 'PDF to Markdown', description: 'Export text by page', input: 'PDF', output: 'MD' },
  { kind: 'pdf_to_rtf', group: 'pdf', title: 'PDF to RTF', description: 'Rich text output', input: 'PDF', output: 'RTF' },
  { kind: 'pdf_to_flat_odt_xml', group: 'pdf', title: 'PDF to XML', description: 'Flat ODF text XML', input: 'PDF', output: 'XML' },
  { kind: 'pdf_to_pptx', group: 'pdf', title: 'PDF to PPTX', description: 'Editable objects where supported', input: 'PDF', output: 'PPTX' },
  { kind: 'pdf_to_ppt', group: 'pdf', title: 'PDF to PPT', description: 'Legacy presentation output', input: 'PDF', output: 'PPT' },
  { kind: 'pdf_to_odp', group: 'pdf', title: 'PDF to ODP', description: 'Open presentation output', input: 'PDF', output: 'ODP' },
  { kind: 'doc_to_pdf', group: 'word', title: 'DOC to PDF', description: 'Export a legacy Word document', input: 'DOC', output: 'PDF' },
  { kind: 'docx_to_pdf', group: 'word', title: 'DOCX to PDF', description: 'Export document pages', input: 'DOCX', output: 'PDF' },
  { kind: 'odt_to_pdf', group: 'word', title: 'ODT to PDF', description: 'Export an OpenDocument file', input: 'ODT', output: 'PDF' },
  { kind: 'txt_to_pdf', group: 'word', title: 'TXT to PDF', description: 'Typeset UTF-8 text', input: 'TXT', output: 'PDF' },
  { kind: 'rtf_to_pdf', group: 'word', title: 'RTF to PDF', description: 'Export a safe rich text file', input: 'RTF', output: 'PDF' },
  { kind: 'html_to_pdf', group: 'word', title: 'HTML to PDF', description: 'Offline print of one local HTML file', input: 'HTML / HTM', output: 'PDF' },
  { kind: 'markdown_to_pdf', group: 'word', title: 'Markdown to PDF', description: 'Offline Markdown typesetting', input: 'MD / MARKDOWN', output: 'PDF' },
  { kind: 'ppt_to_pdf', group: 'slides', title: 'PPT to PDF', description: 'Export a legacy presentation', input: 'PPT', output: 'PDF' },
  { kind: 'pptx_to_pdf', group: 'slides', title: 'PPTX to PDF', description: 'Export presentation pages', input: 'PPTX', output: 'PDF' },
  { kind: 'odp_to_pdf', group: 'slides', title: 'ODP to PDF', description: 'Export an OpenDocument presentation', input: 'ODP', output: 'PDF' },
  { kind: 'xlsx_to_pdf', group: 'spreadsheets', title: 'XLSX to PDF', description: 'Print a spreadsheet to PDF', input: 'XLSX', output: 'PDF' },
  { kind: 'ods_to_pdf', group: 'spreadsheets', title: 'ODS to PDF', description: 'Print an OpenDocument spreadsheet', input: 'ODS', output: 'PDF' },
  { kind: 'image_to_pdf', group: 'images', title: 'Images to PDF', description: 'Static PNG/JPG/BMP/GIF/WebP/TIFF', input: 'PNG / JPG / BMP / GIF / WEBP / TIFF', output: 'PDF' },
  { kind: 'svg_to_pdf', group: 'images', title: 'SVG to PDF', description: 'Export static vector graphics', input: 'SVG', output: 'PDF' },
];

export function accepts(kind: ConversionKind, file: File) {
  return acceptsName(kind, file.name);
}

export function inputAccept(kind: ConversionKind) {
  if (kind === 'doc_to_pdf') return '.doc';
  if (kind === 'docx_to_pdf') return '.docx';
  if (kind === 'odt_to_pdf') return '.odt';
  if (kind === 'txt_to_pdf') return '.txt';
  if (kind === 'rtf_to_pdf') return '.rtf';
  if (kind === 'html_to_pdf') return '.html,.htm';
  if (kind === 'markdown_to_pdf') return '.md,.markdown';
  if (kind === 'ppt_to_pdf') return '.ppt';
  if (kind === 'pptx_to_pdf') return '.pptx';
  if (kind === 'odp_to_pdf') return '.odp';
  if (kind === 'xlsx_to_pdf') return '.xlsx';
  if (kind === 'ods_to_pdf') return '.ods';
  if (kind === 'image_to_pdf') return '.png,.jpg,.jpeg,.bmp,.gif,.webp,.tif,.tiff';
  if (kind === 'svg_to_pdf') return '.svg';
  return '.pdf';
}

export function acceptsName(kind: ConversionKind, fileName: string) {
  const dot = fileName.lastIndexOf('.');
  return dot >= 0 && inputAccept(kind).split(',').includes(fileName.slice(dot).toLowerCase());
}

export function outputName(kind: ConversionKind, fileName: string, options: ConversionOptions) {
  const stem = fileName.replace(/\.[^.]+$/, '');
  if (kind === 'pdf_to_doc') return `${stem}.doc`;
  if (kind === 'pdf_to_docx') return `${stem}.docx`;
  if (kind === 'pdf_to_odt') return `${stem}.odt`;
  if (kind === 'pdf_to_odp') return `${stem}.odp`;
  if (kind === 'pdf_to_ppt') return `${stem}.ppt`;
  if (kind === 'pdf_to_pptx') return `${stem}.pptx`;
  if (kind === 'pdf_to_rtf') return `${stem}.rtf`;
  if (kind === 'pdf_to_flat_odt_xml') return `${stem}.xml`;
  if (kind === 'pdf_to_markdown') return `${stem}.md`;
  if (kind === 'image_to_pdf' && options.mergeImages) return `${stem}-merged.pdf`;
  if (kind === 'doc_to_pdf' || kind === 'docx_to_pdf' || kind === 'odt_to_pdf' || kind === 'txt_to_pdf' || kind === 'rtf_to_pdf' || kind === 'html_to_pdf' || kind === 'markdown_to_pdf' || kind === 'image_to_pdf' || kind === 'svg_to_pdf' || kind === 'ppt_to_pdf' || kind === 'pptx_to_pdf' || kind === 'odp_to_pdf' || kind === 'xlsx_to_pdf' || kind === 'ods_to_pdf') return `${stem}.pdf`;
  if (kind === 'pdf_to_txt') return `${stem}.txt`;
  return `${stem}.${options.imageFormat ?? 'png'}`;
}
