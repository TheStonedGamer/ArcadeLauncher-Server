import React from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Routes, Route } from 'react-router-dom'
import App from './App.jsx'
import { AuthProvider } from './auth.jsx'
import Home from './pages/Home.jsx'
import GameDetail from './pages/GameDetail.jsx'
import Login from './pages/Login.jsx'
import Register from './pages/Register.jsx'
import Library from './pages/Library.jsx'
import Download from './pages/Download.jsx'
import './styles.css'

createRoot(document.getElementById('root')).render(
  <React.StrictMode>
    <BrowserRouter>
      <AuthProvider>
        <Routes>
          <Route path="/" element={<App />}>
            <Route index element={<Home />} />
            <Route path="game/:id" element={<GameDetail />} />
            <Route path="login" element={<Login />} />
            <Route path="register" element={<Register />} />
            <Route path="library" element={<Library />} />
            <Route path="download" element={<Download />} />
          </Route>
        </Routes>
      </AuthProvider>
    </BrowserRouter>
  </React.StrictMode>
)
