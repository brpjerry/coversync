// Icons for sorts and filter tags; anything without its own falls back to a generic one.

import type { Component } from 'svelte';
import ArrowUpDown from '@lucide/svelte/icons/arrow-up-down';
import CircleAlert from '@lucide/svelte/icons/circle-alert';
import AudioLines from '@lucide/svelte/icons/audio-lines';
import CalendarDays from '@lucide/svelte/icons/calendar-days';
import CaseSensitive from '@lucide/svelte/icons/case-sensitive';
import Clock from '@lucide/svelte/icons/clock';
import Disc from '@lucide/svelte/icons/disc';
import DiscAlbum from '@lucide/svelte/icons/disc-album';
import Feather from '@lucide/svelte/icons/feather';
import Globe from '@lucide/svelte/icons/globe';
import Hash from '@lucide/svelte/icons/hash';
import Languages from '@lucide/svelte/icons/languages';
import MicVocal from '@lucide/svelte/icons/mic-vocal';
import Tag from '@lucide/svelte/icons/tag';
import Timer from '@lucide/svelte/icons/timer';
import UserRound from '@lucide/svelte/icons/user-round';
import UsersRound from '@lucide/svelte/icons/users-round';

type Icon = Component<{ size?: number; strokeWidth?: number }>;

const ICONS: Record<string, Icon> = {
	added: Clock,
	attention: CircleAlert,
	album_artist: MicVocal,
	album_artists: UsersRound,
	artist: UserRound,
	artists: UsersRound,
	album: DiscAlbum,
	title: CaseSensitive,
	date: CalendarDays,
	release_date: CalendarDays,
	original_date: CalendarDays,
	genre: Tag,
	mood: Tag,
	label: Disc,
	publisher: Disc,
	catalog_number: Hash,
	track_number: Hash,
	disc_number: Hash,
	composer: Feather,
	lyricist: Feather,
	writer: Feather,
	language: Languages,
	release_country: Globe,
	bpm: AudioLines,
	length: Timer
};

export const sortIcon = (id: string): Icon => ICONS[id] ?? ArrowUpDown;
