import { useCallback } from 'react';
import {
  ReactFlow,
  addEdge,
  useNodesState,
  useEdgesState,
  type Connection,
  type Node,
  type Edge,
  Controls,
  Background,
  MiniMap,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';

import { ModeToggle } from './components/ModeToggle';
import { AgentNode } from './components/AgentNode';
import { useGraphStore } from './store/graphStore';

const nodeTypes = {
  agent: AgentNode,
};

const initialNodes: Node[] = [
  {
    id: 'researcher',
    type: 'agent',
    position: { x: 100, y: 100 },
    data: { label: 'Researcher', provider: 'openai', model: 'gpt-4o' },
  },
  {
    id: 'summarizer',
    type: 'agent',
    position: { x: 400, y: 100 },
    data: { label: 'Summarizer', provider: 'anthropic', model: 'claude-haiku' },
  },
];

const initialEdges: Edge[] = [
  { id: 'e1', source: 'researcher', target: 'summarizer', animated: true },
];

export function App() {
  const [nodes, _setNodes, onNodesChange] = useNodesState(initialNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(initialEdges);
  const uiMode = useGraphStore((s) => s.uiMode);

  const onConnect = useCallback(
    (connection: Connection) => setEdges((eds) => addEdge(connection, eds)),
    [setEdges]
  );

  return (
    <div style={{ width: '100%', height: '100%', display: 'flex', flexDirection: 'column' }}>
      <ModeToggle />
      <div style={{ flex: 1 }}>
        <ReactFlow
          nodes={nodes}
          edges={edges}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onConnect={onConnect}
          nodeTypes={nodeTypes}
          fitView
        >
          <Controls />
          <Background />
          <MiniMap />
        </ReactFlow>
      </div>
      <div style={{
        padding: '8px 16px',
        background: '#1a1a2e',
        color: '#e0e0e0',
        fontSize: '12px',
        display: 'flex',
        justifyContent: 'space-between',
      }}>
        <span>Nexus v0.1.0</span>
        <span>Mode: {uiMode}</span>
        <span>{nodes.length} agents | {edges.length} connections</span>
      </div>
    </div>
  );
}
