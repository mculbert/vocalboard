import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';

/** Merges Tailwind class strings, resolving conflicts with the last value winning. */
export function cn(...inputs: ClassValue[]): string {
	return twMerge(clsx(inputs));
}
