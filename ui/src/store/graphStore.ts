import { create } from 'zustand';

interface TokenStatus {
  used: number;
  budget: number;
}

interface GraphStore {
  uiMode: 'canvas' | 'builder' | 'developer';
  tokenStatus: Record<string, TokenStatus>;

  setUiMode: (mode: 'canvas' | 'builder' | 'developer') => void;
  updateTokenStatus: (nodeId: string, status: TokenStatus) => void;
}

export const useGraphStore = create<GraphStore>()((set) => ({
  uiMode: 'canvas',
  tokenStatus: {},

  setUiMode: (mode) => set({ uiMode: mode }),
  updateTokenStatus: (nodeId, status) =>
    set((s) => ({
      tokenStatus: { ...s.tokenStatus, [nodeId]: status },
    })),
}));
