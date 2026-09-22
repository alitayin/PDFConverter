import type { Metadata } from 'next';
import './globals.css';

export const metadata: Metadata = {
  title: 'Ayst Arc PDF',
  description: 'Local PDF conversion tool'
};

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
